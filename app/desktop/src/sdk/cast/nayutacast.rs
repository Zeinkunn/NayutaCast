use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use common::{MediaStreamDescription, TransportStrategy};
use parking_lot::Mutex;
use tokio::sync::RwLock;

use crate::sdk::{
    cast::{CastingState, browser_watch_url},
    discovery::{DeviceInfo, DeviceManager},
};

/// Thread-safe handle wrapping a running `NayutacastSender` on a dedicated
/// owner thread.
pub struct SrtSenderHandle {
    pub description: MediaStreamDescription,
    pub client_count: Arc<AtomicUsize>,
    pub stop_flag: Arc<AtomicBool>,
    pub join_handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl SrtSenderHandle {
    pub fn client_count(&self) -> usize {
        self.client_count.load(Ordering::SeqCst)
    }

    pub fn stop(&self) {
        self.stop_flag.store(true, Ordering::SeqCst);
        if let Some(h) = self.join_handle.lock().take() {
            let _ = h.join();
        }
    }
}

impl Drop for SrtSenderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

pub struct NayutacastCastSession {
    handle: Option<SrtSenderHandle>,
    device_mgr: Arc<DeviceManager>,
    target_peer_id: Option<String>,
    sender_name: String,
    stopped: Arc<AtomicBool>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

impl NayutacastCastSession {
    #[allow(clippy::too_many_arguments)]
    pub async fn connect(
        handle: SrtSenderHandle,
        lan_ip: Ipv4Addr,
        srt_port: u16,
        device: &DeviceInfo,
        sender_name: String,
        device_mgr: Arc<DeviceManager>,
        state: Arc<RwLock<CastingState>>,
        http_port: u16,
    ) -> anyhow::Result<Self> {
        let browser_url = browser_watch_url(lan_ip, http_port);
        *state.write().await = CastingState::Connecting {
            protocol: "NayutaCast Native (SRT)".to_string(),
            message: format!("Mengirim sinyal stream SRT ke {}...", device.name),
        };

        let mut desc = handle.description.clone();
        // Never advertise 0.0.0.0 inside ServiceMessage; always publish
        // reachable LAN IPv4.
        let fixed_addr = SocketAddr::new(IpAddr::V4(lan_ip), srt_port);
        desc.transport.strategy = match desc.transport.strategy {
            TransportStrategy::Direct(addr) => {
                if addr.ip().is_unspecified() {
                    TransportStrategy::Direct(SocketAddr::new(IpAddr::V4(lan_ip), addr.port()))
                } else {
                    TransportStrategy::Direct(addr)
                }
            }
            _ => TransportStrategy::Direct(fixed_addr),
        };

        device_mgr
            .announce_stream(
                device.nayutacast_peer_id.as_deref(),
                &sender_name,
                Some(desc.clone()),
            )
            .await?;

        *state.write().await = CastingState::WaitingForReceiver {
            protocol: "NayutaCast Native (SRT)".to_string(),
            message: format!(
                "Menunggu aplikasi receiver {} tersambung ke SRT...",
                device.name
            ),
            browser_url: browser_url.clone(),
        };

        // Re-announce once after 1s and wait up to 8s for actual SRT accept
        // (client_count() > 0).
        let mut connected = false;
        for step in 0..40 {
            if step == 5 {
                let _ = device_mgr
                    .announce_stream(
                        device.nayutacast_peer_id.as_deref(),
                        &sender_name,
                        Some(desc.clone()),
                    )
                    .await;
            }
            if handle.client_count() > 0 {
                connected = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        if !connected {
            let _ = device_mgr
                .announce_stream(device.nayutacast_peer_id.as_deref(), &sender_name, None)
                .await;
            handle.stop();
            anyhow::bail!(
                "Receiver NayutaCast {} tidak menyambung ke SRT dalam 8 detik",
                device.name
            );
        }

        let target_name = device.name.clone();
        *state.write().await = CastingState::Connected {
            protocol: "NayutaCast Native (SRT)".to_string(),
            target_name,
            bytes_sent: 0,
            active_clients: handle.client_count(),
            browser_url,
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_bg = stopped.clone();
        let state_bg = state.clone();
        let count_bg = handle.client_count.clone();

        // Keep polling active client count cleanly while session lives.
        let poll_task = tokio::spawn(async move {
            let mut ticks = 0u64;
            while !stopped_bg.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(1)).await;
                if stopped_bg.load(Ordering::SeqCst) {
                    break;
                }
                let cnt = count_bg.load(Ordering::SeqCst);
                ticks += 1;
                let mut st = state_bg.write().await;
                if let CastingState::Connected {
                    bytes_sent,
                    active_clients,
                    ..
                } = &mut *st
                {
                    *active_clients = cnt;
                    *bytes_sent = ticks;
                }
            }
        });

        Ok(Self {
            handle: Some(handle),
            device_mgr,
            target_peer_id: device.nayutacast_peer_id.clone(),
            sender_name,
            stopped,
            poll_task: Some(poll_task),
        })
    }

    pub async fn stop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(t) = self.poll_task.take() {
            t.abort();
            let _ = t.await;
        }
        let _ = self
            .device_mgr
            .announce_stream(self.target_peer_id.as_deref(), &self.sender_name, None)
            .await;
        if let Some(h) = self.handle.take() {
            h.stop();
        }
    }
}
