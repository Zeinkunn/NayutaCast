use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use url::form_urlencoded;

use crate::sdk::{
    cast::{CastContext, CastingState},
    net_utils::http_request,
};

pub struct RokuSession {
    base_url: String,
    stopped: Arc<AtomicBool>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

impl RokuSession {
    pub async fn connect(ctx: CastContext) -> anyhow::Result<Self> {
        let port = ctx.device.roku_port.unwrap_or(8060);
        let base_url = format!("http://{}:{}", ctx.device.ip, port);

        *ctx.state.write().await = CastingState::Connecting {
            protocol: "Roku TV".to_string(),
            message: format!("Memeriksa channel pemutar media di {}...", ctx.device.name),
        };

        let apps_resp = http_request(
            "GET",
            &format!("{}/query/apps", base_url),
            &[],
            None,
            Duration::from_secs(3),
        )
        .await?;

        let mut app_id = "2213".to_string(); // Default Roku Media Player
        let xml = apps_resp.body_str();
        if let Ok(doc) = roxmltree::Document::parse(&xml) {
            if let Some(node) = doc.descendants().find(|n| {
                n.tag_name().name().eq_ignore_ascii_case("app")
                    && n.text()
                        .map(|t| {
                            let low = t.to_ascii_lowercase();
                            low.contains("media player") || low.contains("playonroku")
                        })
                        .unwrap_or(false)
            }) && let Some(id) = node.attribute("id")
            {
                app_id = id.to_string();
            }
        }

        let stream_url = format!("http://{}:{}/live.mp4", ctx.lan_ip, ctx.http_port);
        let encoded_url: String = form_urlencoded::byte_serialize(stream_url.as_bytes()).collect();
        let launch_url = format!(
            "{}/launch/{}?t=v&u={}&videoName=NayutaCast&videoFormat=mp4",
            base_url, app_id, encoded_url
        );

        let launch_resp =
            http_request("POST", &launch_url, &[], Some(&[]), Duration::from_secs(4)).await?;
        if launch_resp.status >= 400 {
            anyhow::bail!("Roku /launch gagal (HTTP {})", launch_resp.status);
        }

        *ctx.state.write().await = CastingState::WaitingForReceiver {
            protocol: "Roku TV".to_string(),
            message: format!("Menunggu Roku {} menarik stream video...", ctx.device.name),
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
            anyhow::bail!(
                "Roku TV {} tidak mulai memutar URL stream; coba DLNA atau Browser",
                ctx.device.name
            );
        }

        *ctx.state.write().await = CastingState::Connected {
            protocol: "Roku TV".to_string(),
            target_name: ctx.device.name.clone(),
            bytes_sent: ctx.hub.bytes_served(),
            active_clients: ctx.hub.active_clients().max(1),
            browser_url: ctx.browser_url(),
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_bg = stopped.clone();
        let hub_bg = ctx.hub.clone();
        let state_bg = ctx.state.clone();

        let poll_task = tokio::spawn(async move {
            while !stopped_bg.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(2)).await;
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
            &format!("{}/keypress/Home", self.base_url),
            &[],
            Some(&[]),
            Duration::from_secs(2),
        )
        .await;
    }
}
