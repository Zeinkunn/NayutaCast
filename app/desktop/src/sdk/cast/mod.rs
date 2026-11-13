pub mod airplay;
pub mod dlna;
pub mod google_cast;
pub mod nayutacast;
pub mod roku;

use std::{net::Ipv4Addr, sync::Arc, time::Duration};

use tokio::sync::RwLock;

use crate::sdk::{
    discovery::{CastProtocol, DeviceInfo, DeviceManager},
    http_stream::StreamHub,
};

#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum CastingState {
    Idle,
    Connecting {
        protocol: String,
        message: String,
    },
    WaitingForReceiver {
        protocol: String,
        message: String,
        browser_url: String,
    },
    Connected {
        protocol: String,
        target_name: String,
        bytes_sent: u64,
        active_clients: usize,
        browser_url: String,
    },
    Failed {
        error: String,
        tried_protocols: Vec<String>,
        browser_url: String,
    },
}

pub fn browser_watch_url(lan_ip: Ipv4Addr, http_port: u16) -> String {
    format!("http://{}:{}/watch", lan_ip, http_port)
}

#[derive(Clone)]
pub struct CastContext {
    pub device: DeviceInfo,
    pub lan_ip: Ipv4Addr,
    pub http_port: u16,
    pub hub: Arc<StreamHub>,
    pub state: Arc<RwLock<CastingState>>,
}

impl CastContext {
    pub fn browser_url(&self) -> String {
        browser_watch_url(self.lan_ip, self.http_port)
    }
}

pub enum ActiveCastBackend {
    GoogleCast(google_cast::GoogleCastSession),
    Dlna(dlna::DlnaSession),
    AirPlay(airplay::AirPlaySession),
    Roku(roku::RokuSession),
    Nayutacast(nayutacast::NayutacastCastSession),
    BrowserOnly {
        stopped: Arc<std::sync::atomic::AtomicBool>,
        task: Option<tokio::task::JoinHandle<()>>,
    },
}

impl ActiveCastBackend {
    pub async fn stop(&mut self) {
        match self {
            ActiveCastBackend::GoogleCast(s) => s.stop().await,
            ActiveCastBackend::Dlna(s) => s.stop().await,
            ActiveCastBackend::AirPlay(s) => s.stop().await,
            ActiveCastBackend::Roku(s) => s.stop().await,
            ActiveCastBackend::Nayutacast(s) => s.stop().await,
            ActiveCastBackend::BrowserOnly { stopped, task } => {
                stopped.store(true, std::sync::atomic::Ordering::SeqCst);
                if let Some(t) = task.take() {
                    t.abort();
                }
            }
        }
    }
}

/// Attempts smart-TV streaming protocols (excluding NayutaCast SRT which uses
/// its own sender) in priority / preference order and transitions `state`
/// honestly.
pub async fn connect_smart_tv_protocols(
    ctx: CastContext,
    protocols: &[CastProtocol],
) -> anyhow::Result<ActiveCastBackend> {
    let browser_url = ctx.browser_url();
    let mut tried = Vec::new();
    let mut errors = Vec::new();

    for &proto in protocols {
        match proto {
            CastProtocol::NayutacastNative => continue,
            CastProtocol::GoogleCast => {
                tried.push("Google Cast".to_string());
                match google_cast::GoogleCastSession::connect(ctx.clone()).await {
                    Ok(s) => return Ok(ActiveCastBackend::GoogleCast(s)),
                    Err(e) => errors.push(format!("Google Cast: {}", e)),
                }
            }
            CastProtocol::DlnaAvTransport => {
                tried.push("DLNA Smart TV".to_string());
                match dlna::DlnaSession::connect(ctx.clone()).await {
                    Ok(s) => return Ok(ActiveCastBackend::Dlna(s)),
                    Err(e) => errors.push(format!("DLNA: {}", e)),
                }
            }
            CastProtocol::AirPlay => {
                tried.push("AirPlay".to_string());
                match airplay::AirPlaySession::connect(ctx.clone()).await {
                    Ok(s) => return Ok(ActiveCastBackend::AirPlay(s)),
                    Err(e) => errors.push(format!("AirPlay: {}", e)),
                }
            }
            CastProtocol::RokuEcp => {
                tried.push("Roku TV".to_string());
                match roku::RokuSession::connect(ctx.clone()).await {
                    Ok(s) => return Ok(ActiveCastBackend::Roku(s)),
                    Err(e) => errors.push(format!("Roku: {}", e)),
                }
            }
            CastProtocol::BrowserFallback => {
                // Only use BrowserFallback automatically when NO other TV
                // protocol was listed.
                let only_browser = protocols
                    .iter()
                    .all(|p| *p == CastProtocol::BrowserFallback);
                if only_browser {
                    return Ok(start_browser_watch(ctx).await);
                }
            }
        }
    }

    let detail = if errors.is_empty() {
        "Tidak ada protokol TV aktif yang tersedia pada perangkat ini".to_string()
    } else {
        errors.join(" | ")
    };

    *ctx.state.write().await = CastingState::Failed {
        error: detail.clone(),
        tried_protocols: tried,
        browser_url,
    };

    anyhow::bail!(detail)
}

pub async fn start_browser_watch(ctx: CastContext) -> ActiveCastBackend {
    let url = ctx.browser_url();
    *ctx.state.write().await = CastingState::WaitingForReceiver {
        protocol: "Browser TV".to_string(),
        message: format!("Buka tautan berikut di browser Smart TV: {}", url),
        browser_url: url.clone(),
    };

    let stopped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let stopped_bg = stopped.clone();
    let hub_bg = ctx.hub.clone();
    let state_bg = ctx.state.clone();
    let target_name = ctx.device.name.clone();

    let task = tokio::spawn(async move {
        while !stopped_bg.load(std::sync::atomic::Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(500)).await;
            let clients = hub_bg.active_clients();
            let bytes = hub_bg.bytes_served();
            if clients > 0 {
                *state_bg.write().await = CastingState::Connected {
                    protocol: "Browser TV".to_string(),
                    target_name: target_name.clone(),
                    bytes_sent: bytes,
                    active_clients: clients,
                    browser_url: url.clone(),
                };
            }
        }
    });

    ActiveCastBackend::BrowserOnly {
        stopped,
        task: Some(task),
    }
}

#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub async fn connect_nayutacast_protocol(
    handle: nayutacast::SrtSenderHandle,
    lan_ip: Ipv4Addr,
    srt_port: u16,
    device: &DeviceInfo,
    sender_name: String,
    device_mgr: Arc<DeviceManager>,
    state: Arc<RwLock<CastingState>>,
    http_port: u16,
) -> anyhow::Result<ActiveCastBackend> {
    let sess = nayutacast::NayutacastCastSession::connect(
        handle,
        lan_ip,
        srt_port,
        device,
        sender_name,
        device_mgr,
        state,
        http_port,
    )
    .await?;
    Ok(ActiveCastBackend::Nayutacast(sess))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sdk::{discovery::DeviceKind, http_stream::HttpStreamServer};
    use futures_rustls::TlsAcceptor;
    use prost::Message;
    use rustls::ServerConfig;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::net::IpAddr;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

    #[tokio::test]
    async fn integration_mock_dlna_tv_pulls_stream_and_connects() {
        let hub = StreamHub::new();
        hub.publish_raw_test_stream(
            vec![0x47; 188 * 4],
            b"ftypiso6moov".to_vec(),
            b"moofmdat".to_vec(),
        )
        .await;
        let mut http_srv = HttpStreamServer::start(Ipv4Addr::LOCALHOST, hub.clone())
            .await
            .expect("start http server");

        let tv_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tv_port = tv_listener.local_addr().unwrap().port();
        let saw_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_stop_srv = saw_stop.clone();

        let _tv_task = tokio::spawn(async move {
            let mut held_stream = None;
            for _ in 0..8 {
                let Ok((mut sock, _)) = tv_listener.accept().await else {
                    break;
                };
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();

                let body_xml = if req.contains("SetAVTransportURI") {
                    if let Some(uri_start) = req.find("<CurrentURI>").map(|i| i + 12)
                        && let Some(uri_end) = req[uri_start..].find("</CurrentURI>")
                    {
                        let pull_url = &req[uri_start..uri_start + uri_end];
                        let stripped = pull_url.strip_prefix("http://").unwrap();
                        let (host_port, path) = stripped.split_once('/').unwrap();
                        let mut c = tokio::net::TcpStream::connect(host_port).await.unwrap();
                        c.write_all(
                            format!("GET /{} HTTP/1.1\r\nHost: {}\r\n\r\n", path, host_port)
                                .as_bytes(),
                        )
                        .await
                        .unwrap();
                        let mut first_bytes = [0u8; 256];
                        let _ = c.read(&mut first_bytes).await;
                        held_stream = Some(c);
                    }
                    "<u:SetAVTransportURIResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>"
                } else if req.contains("GetTransportInfo") {
                    "<u:GetTransportInfoResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"><CurrentTransportState>PLAYING</CurrentTransportState></u:GetTransportInfoResponse>"
                } else if req.contains("#Stop") || req.contains("u:Stop") {
                    saw_stop_srv.store(true, std::sync::atomic::Ordering::SeqCst);
                    "<u:StopResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>"
                } else {
                    "<u:PlayResponse xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\"/>"
                };

                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/xml\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body_xml.len(),
                    body_xml
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            }
            drop(held_stream);
        });

        let dev = DeviceInfo {
            id: "ip:127.0.0.1".into(),
            name: "Mock Samsung Smart TV".into(),
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: tv_port,
            network: DeviceKind::Wifi,
            manufacturer: Some("Samsung".into()),
            model_name: Some("AU7000".into()),
            protocols: vec![CastProtocol::DlnaAvTransport],
            av_transport_control_url: Some(format!(
                "http://127.0.0.1:{}/upnp/control/AVTransport1",
                tv_port
            )),
            connection_mgr_control_url: None,
            location_url: None,
            cast_port: None,
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: None,
            is_custom: true,
        };

        let state = Arc::new(RwLock::new(CastingState::Idle));
        let mut backend = connect_smart_tv_protocols(
            CastContext {
                device: dev,
                lan_ip: Ipv4Addr::LOCALHOST,
                http_port: http_srv.port,
                hub: hub.clone(),
                state: state.clone(),
            },
            &[CastProtocol::DlnaAvTransport],
        )
        .await
        .expect("DLNA connect succeeds");

        match &*state.read().await {
            CastingState::Connected { bytes_sent, .. } => assert!(*bytes_sent > 0),
            other => panic!("expected Connected, got {:?}", other),
        }

        backend.stop().await;
        http_srv.stop();
        assert!(saw_stop.load(std::sync::atomic::Ordering::SeqCst));
    }

    #[tokio::test]
    async fn integration_mock_google_cast_tls_launches_and_loads_stream() {
        let dir = std::env::temp_dir().join(format!("nayuta_cast_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let key_pem = dir.join("key.pem");
        let cert_der_path = dir.join("cert.der");
        let key_der_path = dir.join("key.der");

        let st = std::process::Command::new("openssl")
            .args([
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-subj",
                "/CN=127.0.0.1",
                "-days",
                "1",
                "-keyout",
                key_pem.to_str().unwrap(),
                "-out",
                cert_der_path.to_str().unwrap(),
                "-outform",
                "DER",
            ])
            .output()
            .expect("run openssl req");
        assert!(st.status.success(), "openssl req failed: {:?}", st);

        let st2 = std::process::Command::new("openssl")
            .args([
                "pkcs8",
                "-topk8",
                "-nocrypt",
                "-in",
                key_pem.to_str().unwrap(),
                "-out",
                key_der_path.to_str().unwrap(),
                "-outform",
                "DER",
            ])
            .output()
            .expect("run openssl pkcs8");
        assert!(st2.status.success());

        let cert_der = std::fs::read(&cert_der_path).unwrap();
        let key_der = std::fs::read(&key_der_path).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        let server_cfg =
            ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()
                .unwrap()
                .with_no_client_auth()
                .with_single_cert(
                    vec![CertificateDer::from(cert_der)],
                    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key_der)),
                )
                .unwrap();

        let acceptor = TlsAcceptor::from(Arc::new(server_cfg));
        let cast_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let cast_port = cast_listener.local_addr().unwrap().port();

        let hub = StreamHub::new();
        hub.publish_raw_test_stream(
            vec![0x47; 188 * 4],
            b"ftypiso6moov".to_vec(),
            b"moofmdat".to_vec(),
        )
        .await;
        let mut http_srv = HttpStreamServer::start(Ipv4Addr::LOCALHOST, hub.clone())
            .await
            .unwrap();

        let _mock_cast = tokio::spawn(async move {
            let Ok((tcp, _)) = cast_listener.accept().await else {
                return;
            };
            let Ok(tls) = acceptor.accept(TokioAsyncReadCompatExt::compat(tcp)).await else {
                return;
            };
            let mut stream = FuturesAsyncReadCompatExt::compat(tls);
            loop {
                let mut lb = [0u8; 4];
                if stream.read_exact(&mut lb).await.is_err() {
                    break;
                }
                let n = u32::from_be_bytes(lb) as usize;
                let mut body = vec![0u8; n];
                if stream.read_exact(&mut body).await.is_err() {
                    break;
                }
                let msg = google_cast::CastMessage::decode(&body[..]).unwrap();
                let payload = msg.payload_utf8.unwrap_or_default();
                if payload.contains("\"LAUNCH\"") {
                    let reply = google_cast::CastMessage {
                        protocol_version: 0,
                        source_id: "receiver-0".into(),
                        destination_id: "sender-0".into(),
                        namespace: "urn:x-cast:com.google.cast.receiver".into(),
                        payload_type: 0,
                        payload_utf8: Some(
                            r#"{"type":"RECEIVER_STATUS","requestId":2,"status":{"applications":[{"appId":"CC1AD845","sessionId":"sess-1","transportId":"web-99"}]}}"#
                                .into(),
                        ),
                        payload_binary: None,
                    };
                    let enc = reply.encode_to_vec();
                    let _ = stream.write_all(&(enc.len() as u32).to_be_bytes()).await;
                    let _ = stream.write_all(&enc).await;
                    let _ = stream.flush().await;
                } else if payload.contains("\"LOAD\"") {
                    let val: serde_json::Value = serde_json::from_str(&payload).unwrap();
                    let url = val["media"]["contentId"].as_str().unwrap().to_string();
                    let host_port = url
                        .strip_prefix("http://")
                        .unwrap()
                        .split('/')
                        .next()
                        .unwrap();
                    let path = url
                        .strip_prefix("http://")
                        .unwrap()
                        .split_once('/')
                        .unwrap()
                        .1;
                    let mut c = tokio::net::TcpStream::connect(host_port).await.unwrap();
                    c.write_all(
                        format!("GET /{} HTTP/1.1\r\nHost: {}\r\n\r\n", path, host_port).as_bytes(),
                    )
                    .await
                    .unwrap();
                    let mut buf = [0u8; 64];
                    let _ = c.read(&mut buf).await;

                    let reply = google_cast::CastMessage {
                        protocol_version: 0,
                        source_id: "web-99".into(),
                        destination_id: "sender-0".into(),
                        namespace: "urn:x-cast:com.google.cast.media".into(),
                        payload_type: 0,
                        payload_utf8: Some(
                            r#"{"type":"MEDIA_STATUS","requestId":3,"status":[{"mediaSessionId":1,"playerState":"PLAYING"}]}"#
                                .into(),
                        ),
                        payload_binary: None,
                    };
                    let enc = reply.encode_to_vec();
                    let _ = stream.write_all(&(enc.len() as u32).to_be_bytes()).await;
                    let _ = stream.write_all(&enc).await;
                    let _ = stream.flush().await;
                }
            }
        });

        let dev = DeviceInfo {
            id: "ip:127.0.0.1".into(),
            name: "Mock Android TV".into(),
            ip: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: cast_port,
            network: DeviceKind::Wifi,
            manufacturer: Some("Sony".into()),
            model_name: Some("BRAVIA".into()),
            protocols: vec![CastProtocol::GoogleCast],
            av_transport_control_url: None,
            connection_mgr_control_url: None,
            location_url: None,
            cast_port: Some(cast_port),
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: None,
            is_custom: true,
        };

        let state = Arc::new(RwLock::new(CastingState::Idle));
        let mut backend = connect_smart_tv_protocols(
            CastContext {
                device: dev,
                lan_ip: Ipv4Addr::LOCALHOST,
                http_port: http_srv.port,
                hub: hub.clone(),
                state: state.clone(),
            },
            &[CastProtocol::GoogleCast],
        )
        .await
        .expect("Google Cast TLS connect succeeds");

        assert!(matches!(
            &*state.read().await,
            CastingState::Connected { .. }
        ));
        backend.stop().await;
        http_srv.stop();
    }
}
