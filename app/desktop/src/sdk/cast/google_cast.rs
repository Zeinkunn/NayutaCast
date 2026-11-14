use prost::Message;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, DigitallySignedStruct, Error as TlsError, SignatureScheme};
use std::{
    net::{IpAddr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Notify;
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};

use crate::sdk::cast::{CastContext, CastingState};

#[derive(Clone, PartialEq, prost::Message)]
pub struct CastMessage {
    #[prost(int32, required, tag = "1")]
    pub protocol_version: i32,
    #[prost(string, required, tag = "2")]
    pub source_id: String,
    #[prost(string, required, tag = "3")]
    pub destination_id: String,
    #[prost(string, required, tag = "4")]
    pub namespace: String,
    #[prost(int32, required, tag = "5")]
    pub payload_type: i32,
    #[prost(string, optional, tag = "6")]
    pub payload_utf8: Option<String>,
    #[prost(bytes = "vec", optional, tag = "7")]
    pub payload_binary: Option<Vec<u8>>,
}

#[derive(Debug)]
struct TrustAnyCastCert {
    supported: Vec<SignatureScheme>,
}

impl TrustAnyCastCert {
    fn new() -> Self {
        let provider = rustls::crypto::ring::default_provider();
        Self {
            supported: provider
                .signature_verification_algorithms
                .supported_schemes(),
        }
    }
}

impl ServerCertVerifier for TrustAnyCastCert {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        let provider = rustls::crypto::ring::default_provider();
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        let provider = rustls::crypto::ring::default_provider();
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.supported.clone()
    }
}

async fn write_cast_msg<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    src: &str,
    dst: &str,
    ns: &str,
    json_payload: String,
) -> anyhow::Result<()> {
    let msg = CastMessage {
        protocol_version: 0,
        source_id: src.to_string(),
        destination_id: dst.to_string(),
        namespace: ns.to_string(),
        payload_type: 0,
        payload_utf8: Some(json_payload),
        payload_binary: None,
    };
    let encoded = msg.encode_to_vec();
    let len_be = (encoded.len() as u32).to_be_bytes();
    writer.write_all(&len_be).await?;
    writer.write_all(&encoded).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_cast_msg<R: AsyncReadExt + Unpin>(reader: &mut R) -> anyhow::Result<CastMessage> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let msg_len = u32::from_be_bytes(len_buf) as usize;
    if msg_len == 0 || msg_len > 1024 * 1024 {
        anyhow::bail!("invalid CastMessage frame length {}", msg_len);
    }
    let mut body = vec![0u8; msg_len];
    reader.read_exact(&mut body).await?;
    let msg = CastMessage::decode(&body[..])?;
    Ok(msg)
}

pub struct GoogleCastSession {
    stopped: Arc<AtomicBool>,
    stop_notify: Arc<Notify>,
    task: Option<tokio::task::JoinHandle<()>>,
    tls_target: SocketAddr,
    session_id: String,
    transport_id: String,
}

impl GoogleCastSession {
    pub async fn connect(ctx: CastContext) -> anyhow::Result<Self> {
        let port = ctx.device.cast_port.unwrap_or(8009);
        let addr = SocketAddr::new(ctx.device.ip, port);

        *ctx.state.write().await = CastingState::Connecting {
            protocol: "Google Cast".to_string(),
            message: format!("Membuka koneksi TLS Cast v2 ke {}...", addr),
        };

        let mut root_cfg =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(TrustAnyCastCert::new()))
                .with_no_client_auth();
        root_cfg.alpn_protocols = Vec::new();

        let connector = futures_rustls::TlsConnector::from(Arc::new(root_cfg));
        let tcp = tokio::time::timeout(Duration::from_secs(4), TcpStream::connect(addr)).await??;
        let _ = tcp.set_nodelay(true);

        let sni = match ctx.device.ip {
            IpAddr::V4(v4) => ServerName::IpAddress(v4.into()),
            IpAddr::V6(v6) => ServerName::IpAddress(v6.into()),
        };

        let tls_futures = tokio::time::timeout(
            Duration::from_secs(4),
            connector.connect(sni, TokioAsyncReadCompatExt::compat(tcp)),
        )
        .await??;
        let mut tls = FuturesAsyncReadCompatExt::compat(tls_futures);
        log::info!(
            "Google Cast TLS connected to {} ({})",
            ctx.device.name,
            addr
        );

        let req_seq = AtomicU64::new(1);

        // 1. Virtual connection to receiver-0
        write_cast_msg(
            &mut tls,
            "sender-0",
            "receiver-0",
            "urn:x-cast:com.google.cast.tp.connection",
            r#"{"type":"CONNECT","origin":{}}"#.to_string(),
        )
        .await?;

        // Query current receiver status first so an already-running
        // Default Media Receiver (CC1AD845) can be reused without relaunching.
        let status_id = req_seq.fetch_add(1, Ordering::Relaxed);
        write_cast_msg(
            &mut tls,
            "sender-0",
            "receiver-0",
            "urn:x-cast:com.google.cast.receiver",
            format!(r#"{{"type":"GET_STATUS","requestId":{}}}"#, status_id),
        )
        .await?;

        *ctx.state.write().await = CastingState::WaitingForReceiver {
            protocol: "Google Cast".to_string(),
            message: format!("Meluncurkan media receiver di {}...", ctx.device.name),
            browser_url: ctx.browser_url(),
        };

        let mut transport_id: Option<String> = None;
        let mut session_id: Option<String> = None;

        // Check quick GET_STATUS response (~800ms) before issuing LAUNCH.
        let pre_deadline = tokio::time::Instant::now() + Duration::from_millis(800);
        while tokio::time::Instant::now() < pre_deadline && transport_id.is_none() {
            let remain = pre_deadline.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(remain, read_cast_msg(&mut tls)).await {
                Ok(Ok(msg)) => {
                    if msg.namespace == "urn:x-cast:com.google.cast.tp.heartbeat" {
                        let _ = write_cast_msg(
                            &mut tls,
                            "sender-0",
                            "receiver-0",
                            "urn:x-cast:com.google.cast.tp.heartbeat",
                            r#"{"type":"PONG"}"#.to_string(),
                        )
                        .await;
                        continue;
                    }
                    if let Some(payload) = msg.payload_utf8
                        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&payload)
                    {
                        log::debug!(
                            "Google Cast pre-launch msg [{}]: {}",
                            msg.namespace,
                            payload
                        );
                        if val.get("type").and_then(|v| v.as_str()) == Some("RECEIVER_STATUS") {
                            if let Some(apps) = val
                                .get("status")
                                .and_then(|s| s.get("applications"))
                                .and_then(|a| a.as_array())
                                && let Some(app) = apps.iter().find(|a| {
                                    a.get("appId").and_then(|v| v.as_str()) == Some("CC1AD845")
                                })
                            {
                                transport_id = app
                                    .get("transportId")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                session_id = app
                                    .get("sessionId")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                            }
                            if val.get("requestId").and_then(|v| v.as_u64()) == Some(status_id) {
                                break;
                            }
                        }
                    }
                }
                Ok(Err(e)) => {
                    anyhow::bail!("Google Cast connection closed during GET_STATUS: {}", e);
                }
                Err(_) => break,
            }
        }

        if transport_id.is_none() {
            // 2. Launch Default Media Receiver (CC1AD845)
            let launch_id = req_seq.fetch_add(1, Ordering::Relaxed);
            log::info!("Sending Google Cast LAUNCH CC1AD845 to {}", ctx.device.name);
            write_cast_msg(
                &mut tls,
                "sender-0",
                "receiver-0",
                "urn:x-cast:com.google.cast.receiver",
                format!(
                    r#"{{"type":"LAUNCH","appId":"CC1AD845","requestId":{}}}"#,
                    launch_id
                ),
            )
            .await?;

            // Android TV cold-boot of WebView/Default Media Receiver can take
            // >10s.
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);

            while tokio::time::Instant::now() < deadline && transport_id.is_none() {
                let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
                let msg = match tokio::time::timeout(remain, read_cast_msg(&mut tls)).await {
                    Ok(Ok(m)) => m,
                    Ok(Err(e)) => {
                        log::warn!("Google Cast read error while waiting LAUNCH: {}", e);
                        break;
                    }
                    Err(_) => {
                        log::warn!("Google Cast LAUNCH deadline (20s) expired");
                        break;
                    }
                };
                if msg.namespace == "urn:x-cast:com.google.cast.tp.heartbeat" {
                    let _ = write_cast_msg(
                        &mut tls,
                        "sender-0",
                        "receiver-0",
                        "urn:x-cast:com.google.cast.tp.heartbeat",
                        r#"{"type":"PONG"}"#.to_string(),
                    )
                    .await;
                    continue;
                }
                if let Some(payload) = msg.payload_utf8
                    && let Ok(val) = serde_json::from_str::<serde_json::Value>(&payload)
                {
                    log::debug!("Google Cast launch msg [{}]: {}", msg.namespace, payload);
                    let msg_type = val.get("type").and_then(|v| v.as_str()).unwrap_or("");
                    if msg_type == "LAUNCH_ERROR" {
                        let reason = val
                            .get("reason")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        anyhow::bail!(
                            "Google Cast device {} menolak meluncurkan receiver ({})",
                            ctx.device.name,
                            reason
                        );
                    }
                    if msg_type == "RECEIVER_STATUS"
                        && let Some(apps) = val
                            .get("status")
                            .and_then(|s| s.get("applications"))
                            .and_then(|a| a.as_array())
                        && let Some(app) = apps
                            .iter()
                            .find(|a| a.get("appId").and_then(|v| v.as_str()) == Some("CC1AD845"))
                            .or_else(|| apps.first())
                    {
                        transport_id = app
                            .get("transportId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                        session_id = app
                            .get("sessionId")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string());
                    }
                }
            }
        }

        let transport_id = transport_id.ok_or_else(|| {
            anyhow::anyhow!(
                "Google Cast device {} did not launch media receiver",
                ctx.device.name
            )
        })?;
        let session_id = session_id.unwrap_or_default();
        log::info!(
            "Google Cast receiver ready on {} (transportId={}, sessionId={})",
            ctx.device.name,
            transport_id,
            session_id
        );

        // 3. Connect virtual transport channel
        write_cast_msg(
            &mut tls,
            "sender-0",
            &transport_id,
            "urn:x-cast:com.google.cast.tp.connection",
            r#"{"type":"CONNECT","origin":{}}"#.to_string(),
        )
        .await?;

        // 4. LOAD live stream (/live.mp4 primary, /live.m3u8 fallback)
        let candidates = [
            (
                format!("http://{}:{}/live.mp4", ctx.lan_ip, ctx.http_port),
                "video/mp4",
            ),
            (
                format!("http://{}:{}/live.m3u8", ctx.lan_ip, ctx.http_port),
                "application/x-mpegurl",
            ),
        ];

        let mut loaded_ok = false;
        let mut last_err = format!(
            "Google Cast TV tidak dapat mengakses stream HTTP {}:{} (periksa firewall lokal)",
            ctx.lan_ip, ctx.http_port
        );

        for (media_url, mime) in candidates {
            let start_bytes = ctx.hub.bytes_served();
            let req_id = req_seq.fetch_add(1, Ordering::Relaxed);
            log::info!(
                "Sending Google Cast LOAD to {}: {} ({})",
                ctx.device.name,
                media_url,
                mime
            );
            let mut media_obj = serde_json::json!({
                "contentId": media_url,
                "contentType": mime,
                "streamType": "LIVE",
                "metadata": {
                    "metadataType": 0,
                    "title": "NayutaCast Live Screen"
                }
            });
            if mime == "application/x-mpegurl"
                && let Some(obj) = media_obj.as_object_mut()
            {
                obj.insert("hlsSegmentFormat".into(), serde_json::json!("TS"));
                obj.insert(
                    "hlsVideoSegmentFormat".into(),
                    serde_json::json!("MPEG2_TS"),
                );
            }
            let load_json = serde_json::json!({
                "type": "LOAD",
                "requestId": req_id,
                "sessionId": session_id,
                "autoplay": true,
                "currentTime": 0,
                "media": media_obj
            })
            .to_string();

            write_cast_msg(
                &mut tls,
                "sender-0",
                &transport_id,
                "urn:x-cast:com.google.cast.media",
                load_json,
            )
            .await?;

            let load_started = tokio::time::Instant::now();
            let load_deadline = load_started + Duration::from_secs(9);
            let mut saw_buffering = false;
            while tokio::time::Instant::now() < load_deadline {
                if saw_buffering
                    && load_started.elapsed() >= Duration::from_secs(3)
                    && ctx.hub.active_clients() > 0
                    && ctx.hub.bytes_served() >= start_bytes + 4096
                {
                    log::info!(
                        "Google Cast confirmed playback via BUFFERING + active transfer ({} bytes) for {}",
                        ctx.hub.bytes_served().saturating_sub(start_bytes),
                        mime
                    );
                    loaded_ok = true;
                    break;
                }
                let remain = load_deadline.saturating_duration_since(tokio::time::Instant::now());
                let poll_slice = remain.min(Duration::from_millis(250));
                if let Ok(Ok(msg)) = tokio::time::timeout(poll_slice, read_cast_msg(&mut tls)).await
                {
                    if msg.namespace == "urn:x-cast:com.google.cast.tp.heartbeat" {
                        let _ = write_cast_msg(
                            &mut tls,
                            "sender-0",
                            "receiver-0",
                            "urn:x-cast:com.google.cast.tp.heartbeat",
                            r#"{"type":"PONG"}"#.to_string(),
                        )
                        .await;
                        continue;
                    }
                    if let Some(payload) = msg.payload_utf8
                        && let Ok(val) = serde_json::from_str::<serde_json::Value>(&payload)
                    {
                        log::debug!("Google Cast media msg [{}]: {}", msg.namespace, payload);
                        match val.get("type").and_then(|t| t.as_str()) {
                            Some("MEDIA_STATUS") => {
                                if let Some(arr) = val.get("status").and_then(|s| s.as_array())
                                    && let Some(first) = arr.first()
                                {
                                    let ps = first
                                        .get("playerState")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("");
                                    let idle_reason = first
                                        .get("idleReason")
                                        .and_then(|s| s.as_str())
                                        .unwrap_or("");
                                    log::info!(
                                        "Google Cast MEDIA_STATUS during LOAD ({}): playerState={}, idleReason={}",
                                        mime,
                                        ps,
                                        idle_reason
                                    );
                                    if ps == "PLAYING" {
                                        loaded_ok = true;
                                        break;
                                    }
                                    if ps == "BUFFERING" {
                                        saw_buffering = true;
                                    }
                                    if ps == "IDLE" && idle_reason == "ERROR" {
                                        last_err = format!(
                                            "Cast player returned IDLE/ERROR for {} ({})",
                                            mime, payload
                                        );
                                        log::warn!("{}", last_err);
                                        break;
                                    }
                                }
                            }
                            Some("LOAD_FAILED") | Some("INVALID_REQUEST") => {
                                last_err = format!("Cast {} ({})", payload, mime);
                                log::warn!("Google Cast LOAD error: {}", last_err);
                                break;
                            }
                            _ => {}
                        }
                    }
                }
            }
            if !loaded_ok
                && last_err.is_empty()
                && ((saw_buffering
                    && ctx.hub.active_clients() > 0
                    && ctx.hub.bytes_served() > start_bytes)
                    || (ctx.hub.active_clients() > 0
                        && ctx.hub.bytes_served() >= start_bytes + 65_536))
            {
                log::info!(
                    "Google Cast confirmed playback via sustained HTTP transfer ({} bytes, buffering={}) for {}",
                    ctx.hub.bytes_served().saturating_sub(start_bytes),
                    saw_buffering,
                    mime
                );
                loaded_ok = true;
            }
            if loaded_ok {
                log::info!(
                    "Google Cast Connected to {} using {} (bytes_served={})",
                    ctx.device.name,
                    mime,
                    ctx.hub.bytes_served()
                );
                break;
            }
        }

        if !loaded_ok {
            anyhow::bail!(last_err);
        }

        *ctx.state.write().await = CastingState::Connected {
            protocol: "Google Cast".to_string(),
            target_name: ctx.device.name.clone(),
            bytes_sent: ctx.hub.bytes_served(),
            active_clients: ctx.hub.active_clients().max(1),
            browser_url: ctx.browser_url(),
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_bg = stopped.clone();
        let stop_notify = Arc::new(Notify::new());
        let notify_bg = stop_notify.clone();
        let hub_bg = ctx.hub.clone();
        let state_bg = ctx.state.clone();
        let dev_name = ctx.device.name.clone();
        let browser_url = ctx.browser_url();
        let tid_bg = transport_id.clone();
        let sid_bg = session_id.clone();

        let task = tokio::spawn(async move {
            let mut ping_iv = tokio::time::interval(Duration::from_secs(5));
            while !stopped_bg.load(Ordering::SeqCst) {
                tokio::select! {
                    _ = notify_bg.notified() => break,
                    _ = ping_iv.tick() => {
                        if write_cast_msg(
                            &mut tls,
                            "sender-0",
                            "receiver-0",
                            "urn:x-cast:com.google.cast.tp.heartbeat",
                            r#"{"type":"PING"}"#.to_string(),
                        ).await.is_err() {
                            break;
                        }
                        let mut st = state_bg.write().await;
                        if let CastingState::Connected { bytes_sent, active_clients, .. } = &mut *st {
                            *bytes_sent = hub_bg.bytes_served();
                            *active_clients = hub_bg.active_clients().max(1);
                        }
                    }
                    res = read_cast_msg(&mut tls) => {
                        match res {
                            Ok(msg) if msg.namespace == "urn:x-cast:com.google.cast.tp.heartbeat" => {
                                let _ = write_cast_msg(
                                    &mut tls,
                                    "sender-0",
                                    "receiver-0",
                                    "urn:x-cast:com.google.cast.tp.heartbeat",
                                    r#"{"type":"PONG"}"#.to_string(),
                                ).await;
                            }
                            Ok(msg) if msg.namespace == "urn:x-cast:com.google.cast.tp.connection" => {
                                if msg.payload_utf8.as_deref().is_some_and(|p| p.contains("\"CLOSE\"")) {
                                    break;
                                }
                            }
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                }
            }
            let _ = write_cast_msg(
                &mut tls,
                "sender-0",
                &tid_bg,
                "urn:x-cast:com.google.cast.media",
                format!(
                    r#"{{"type":"STOP","requestId":999,"sessionId":"{}"}}"#,
                    sid_bg
                ),
            )
            .await;
            let _ = write_cast_msg(
                &mut tls,
                "sender-0",
                "receiver-0",
                "urn:x-cast:com.google.cast.receiver",
                format!(
                    r#"{{"type":"STOP","requestId":1000,"sessionId":"{}"}}"#,
                    sid_bg
                ),
            )
            .await;
            let _ = (dev_name, browser_url);
        });

        Ok(Self {
            stopped,
            stop_notify,
            task: Some(task),
            tls_target: addr,
            session_id,
            transport_id,
        })
    }

    pub async fn stop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        self.stop_notify.notify_waiters();
        if let Some(mut t) = self.task.take()
            && tokio::time::timeout(Duration::from_millis(600), &mut t)
                .await
                .is_err()
        {
            t.abort();
        }
        let _ = (&self.tls_target, &self.session_id, &self.transport_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cast_message_round_trip_preserves_required_zero_enums() {
        let msg = CastMessage {
            protocol_version: 0,
            source_id: "sender-0".to_string(),
            destination_id: "receiver-0".to_string(),
            namespace: "urn:x-cast:com.google.cast.tp.connection".to_string(),
            payload_type: 0,
            payload_utf8: Some(r#"{"type":"CONNECT"}"#.to_string()),
            payload_binary: None,
        };
        let encoded = msg.encode_to_vec();
        // Required tag 1 = 0 must appear as [0x08, 0x00]; required tag 5 = 0
        // must appear as [0x28, 0x00].
        assert!(
            encoded.windows(2).any(|w| w == [0x08, 0x00]),
            "missing protocol_version tag 1"
        );
        assert!(
            encoded.windows(2).any(|w| w == [0x28, 0x00]),
            "missing payload_type tag 5"
        );

        let decoded = CastMessage::decode(&encoded[..]).expect("decode CastMessage");
        assert_eq!(decoded, msg);
    }
}
