use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::sdk::{
    cast::{CastContext, CastingState},
    discovery::probe_airplay_info,
    net_utils::http_request,
};

pub struct AirPlaySession {
    base_url: String,
    session_id: String,
    stopped: Arc<AtomicBool>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

fn generate_airplay_session_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "{:08x}-{:04x}-4{:03x}-8{:03x}-{:012x}",
        (nanos & 0xffff_ffff) as u32,
        ((nanos >> 32) & 0xffff) as u16,
        ((nanos >> 48) & 0x0fff) as u16,
        ((nanos >> 60) & 0x0fff) as u16,
        ((nanos >> 16) ^ nanos) & 0xffff_ffff_ffff
    )
}

impl AirPlaySession {
    pub async fn connect(ctx: CastContext) -> anyhow::Result<Self> {
        let port = ctx.device.airplay_port.unwrap_or(7000);
        let base_url = format!("http://{}:{}", ctx.device.ip, port);
        let session_id = generate_airplay_session_id();
        let device_id = "02:00:00:4e:59:54";

        *ctx.state.write().await = CastingState::Connecting {
            protocol: "Apple AirPlay".to_string(),
            message: format!("Memeriksa endpoint AirPlay di {}...", base_url),
        };

        let endpoint_info = probe_airplay_info(ctx.device.ip, port).await;
        let server_info_resp = if endpoint_info.is_none() {
            Some(
                http_request(
                    "GET",
                    &format!("{}/server-info", base_url),
                    &[
                        ("User-Agent", "MediaControl/1.0"),
                        ("X-Apple-Session-ID", &session_id),
                    ],
                    None,
                    Duration::from_millis(1200),
                )
                .await,
            )
        } else {
            None
        };

        let hls_url = format!("http://{}:{}/live.m3u8", ctx.lan_ip, ctx.http_port);
        let mp4_url = format!("http://{}:{}/live.mp4", ctx.lan_ip, ctx.http_port);
        let text_hls = format!("Content-Location: {}\nStart-Position: 0.0\n", hls_url);
        let plist_hls = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\"><dict><key>Content-Location</key><string>{}</string><key>Start-Position</key><real>0.0</real></dict></plist>\n",
            hls_url
        );
        let text_mp4 = format!("Content-Location: {}\nStart-Position: 0.0\n", mp4_url);

        let candidates = [
            ("text/parameters", "MediaControl/1.0", text_hls.as_bytes()),
            (
                "text/x-apple-plist+xml",
                "AirPlay/540.31",
                plist_hls.as_bytes(),
            ),
            ("text/parameters", "MediaControl/1.0", text_mp4.as_bytes()),
        ];

        let mut last_auth_err: Option<String> = None;
        let mut last_other_err: Option<String> = None;
        let mut verified = false;

        for (content_type, user_agent, payload) in candidates {
            let play_resp = match http_request(
                "POST",
                &format!("{}/play", base_url),
                &[
                    ("Content-Type", content_type),
                    ("User-Agent", user_agent),
                    ("X-Apple-Session-ID", &session_id),
                    ("X-Apple-Device-ID", device_id),
                ],
                Some(payload),
                Duration::from_secs(4),
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    last_other_err = Some(e.to_string());
                    continue;
                }
            };

            if play_resp.status == 401 || play_resp.status == 403 {
                last_auth_err = Some(format!(
                    "AirPlay pada {} menolak /play (HTTP {}) karena memerlukan autentikasi PIN/FairPlay",
                    ctx.device.name, play_resp.status
                ));
                break;
            }
            if play_resp.status >= 400 {
                last_other_err = Some(format!(
                    "AirPlay /play gagal (HTTP {}): {}",
                    play_resp.status,
                    play_resp.body_str()
                ));
                continue;
            }

            // Some Android TV AirPlay receivers start paused until /rate?value=1 is sent.
            let _ = http_request(
                "POST",
                &format!("{}/rate?value=1.000000", base_url),
                &[
                    ("User-Agent", user_agent),
                    ("X-Apple-Session-ID", &session_id),
                ],
                None,
                Duration::from_secs(2),
            )
            .await;

            *ctx.state.write().await = CastingState::WaitingForReceiver {
                protocol: "Apple AirPlay".to_string(),
                message: format!("Menunggu {} memuat stream HLS AirPlay...", ctx.device.name),
                browser_url: ctx.browser_url(),
            };

            for _ in 0..24 {
                if ctx.hub.active_clients() > 0 && ctx.hub.bytes_served() > 0 {
                    verified = true;
                    break;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            }

            if verified {
                break;
            }

            let _ = http_request(
                "POST",
                &format!("{}/stop", base_url),
                &[("X-Apple-Session-ID", &session_id)],
                None,
                Duration::from_secs(2),
            )
            .await;
            last_other_err = Some(format!(
                "TV AirPlay {} tidak mulai menarik stream HTTP dalam 6 detik",
                ctx.device.name
            ));
        }

        if !verified {
            if let Some(auth_msg) = last_auth_err {
                let extra = endpoint_info
                    .as_ref()
                    .and_then(|i| i.model.as_deref())
                    .map(|m| format!(" (model: {})", m))
                    .unwrap_or_default();
                anyhow::bail!("{}{}", auth_msg, extra);
            }
            if let Some(Ok(resp)) = &server_info_resp
                && (resp.status == 401 || resp.status == 403)
                && last_other_err.is_none()
            {
                anyhow::bail!(
                    "AirPlay pada {} memerlukan autentikasi FairPlay/PIN (HTTP {})",
                    ctx.device.name,
                    resp.status
                );
            }
            anyhow::bail!(
                "{}",
                last_other_err
                    .unwrap_or_else(|| format!("AirPlay ke {} gagal menyambung", ctx.device.name))
            );
        }

        *ctx.state.write().await = CastingState::Connected {
            protocol: "Apple AirPlay".to_string(),
            target_name: ctx.device.name.clone(),
            bytes_sent: ctx.hub.bytes_served(),
            active_clients: ctx.hub.active_clients().max(1),
            browser_url: ctx.browser_url(),
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_bg = stopped.clone();
        let hub_bg = ctx.hub.clone();
        let state_bg = ctx.state.clone();
        let url_bg = base_url.clone();
        let sid_bg = session_id.clone();

        let poll_task = tokio::spawn(async move {
            while !stopped_bg.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if stopped_bg.load(Ordering::SeqCst) {
                    break;
                }
                let _ = http_request(
                    "GET",
                    &format!("{}/playback-info", url_bg),
                    &[
                        ("User-Agent", "MediaControl/1.0"),
                        ("X-Apple-Session-ID", &sid_bg),
                    ],
                    None,
                    Duration::from_secs(2),
                )
                .await;
                let mut st = state_bg.write().await;
                if let CastingState::Connected {
                    bytes_sent,
                    active_clients,
                    ..
                } = &mut *st
                {
                    *bytes_sent = hub_bg.bytes_served();
                    *active_clients = hub_bg.active_clients();
                }
            }
        });

        Ok(Self {
            base_url,
            session_id,
            stopped,
            poll_task: Some(poll_task),
        })
    }

    pub async fn stop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(t) = self.poll_task.take() {
            t.abort();
        }
        let _ = http_request(
            "POST",
            &format!("{}/stop", self.base_url),
            &[("X-Apple-Session-ID", &self.session_id)],
            None,
            Duration::from_secs(2),
        )
        .await;
    }
}
