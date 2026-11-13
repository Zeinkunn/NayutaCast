use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::sdk::{
    cast::{CastContext, CastingState},
    net_utils::http_request,
};

pub struct AirPlaySession {
    base_url: String,
    stopped: Arc<AtomicBool>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

impl AirPlaySession {
    pub async fn connect(ctx: CastContext) -> anyhow::Result<Self> {
        let port = ctx.device.airplay_port.unwrap_or(7000);
        let base_url = format!("http://{}:{}", ctx.device.ip, port);

        *ctx.state.write().await = CastingState::Connecting {
            protocol: "Apple AirPlay".to_string(),
            message: format!("Memeriksa endpoint AirPlay di {}...", base_url),
        };

        // Check reachability (/server-info or /info)
        let probe_res = http_request(
            "GET",
            &format!("{}/server-info", base_url),
            &[("User-Agent", "MediaControl/1.0")],
            None,
            Duration::from_secs(3),
        )
        .await;

        if let Ok(resp) = &probe_res
            && (resp.status == 401 || resp.status == 403)
        {
            anyhow::bail!(
                "AirPlay pada {} memerlukan autentikasi FairPlay/PIN (HTTP {})",
                ctx.device.name,
                resp.status
            );
        }

        let hls_url = format!("http://{}:{}/live.m3u8", ctx.lan_ip, ctx.http_port);
        let play_body = format!("Content-Location: {}\nStart-Position: 0.0\n", hls_url);

        let play_resp = http_request(
            "POST",
            &format!("{}/play", base_url),
            &[
                ("Content-Type", "text/parameters"),
                ("User-Agent", "MediaControl/1.0"),
            ],
            Some(play_body.as_bytes()),
            Duration::from_secs(4),
        )
        .await?;

        if play_resp.status == 401 || play_resp.status == 403 {
            anyhow::bail!(
                "AirPlay menolak /play karena memerlukan PIN/FairPlay (HTTP {})",
                play_resp.status
            );
        }
        if play_resp.status >= 400 {
            anyhow::bail!(
                "AirPlay /play gagal (HTTP {}): {}",
                play_resp.status,
                play_resp.body_str()
            );
        }

        *ctx.state.write().await = CastingState::WaitingForReceiver {
            protocol: "Apple AirPlay".to_string(),
            message: format!("Menunggu {} memuat stream HLS AirPlay...", ctx.device.name),
            browser_url: ctx.browser_url(),
        };

        let mut verified = false;
        for _ in 0..32 {
            if ctx.hub.active_clients() > 0 && ctx.hub.bytes_served() > 0 {
                verified = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        if !verified {
            let _ = http_request(
                "POST",
                &format!("{}/stop", base_url),
                &[],
                None,
                Duration::from_secs(2),
            )
            .await;
            anyhow::bail!(
                "TV AirPlay {} tidak mulai menarik stream HLS dalam 8 detik",
                ctx.device.name
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

        let poll_task = tokio::spawn(async move {
            while !stopped_bg.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if stopped_bg.load(Ordering::SeqCst) {
                    break;
                }
                let _ = http_request(
                    "GET",
                    &format!("{}/playback-info", url_bg),
                    &[("User-Agent", "MediaControl/1.0")],
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
            &[],
            None,
            Duration::from_secs(2),
        )
        .await;
    }
}
