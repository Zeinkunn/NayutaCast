use std::{
    net::IpAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use tokio::sync::RwLock;

use crate::sdk::{
    cast::{CastContext, CastingState},
    discovery::{DeviceManager, parse_upnp_device_xml},
    http_stream::StreamHub,
    net_utils::{http_request, tcp_port_open},
};

fn escape_xml(input: &str) -> String {
    let mut out = String::with_capacity(input.len() + 16);
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

async fn send_soap_action(
    control_url: &str,
    service_type: &str,
    action: &str,
    body_xml: &str,
) -> anyhow::Result<String> {
    let envelope = format!(
        r#"<?xml version="1.0" encoding="utf-8"?>
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/" s:encodingStyle="http://schemas.xmlsoap.org/soap/encoding/">
  <s:Body>
    <u:{action} xmlns:u="{service_type}">
      {body_xml}
    </u:{action}>
  </s:Body>
</s:Envelope>"#
    );
    let soap_action = format!("\"{}#{}\"", service_type, action);
    let resp = http_request(
        "POST",
        control_url,
        &[
            ("Content-Type", "text/xml; charset=\"utf-8\""),
            ("SOAPACTION", &soap_action),
        ],
        Some(envelope.as_bytes()),
        Duration::from_secs(3),
    )
    .await?;

    let text = resp.body_str().into_owned();
    if resp.status >= 400 {
        anyhow::bail!("SOAP {} failed (HTTP {}): {}", action, resp.status, text);
    }
    Ok(text)
}

async fn probe_dlna_control_urls(ip: IpAddr) -> Option<(String, Option<String>)> {
    if let Some(parsed) = DeviceManager::unicast_ssdp_probe(ip).await
        && let Some(av) = parsed.av_transport_control_url
    {
        return Some((av, parsed.connection_mgr_control_url));
    }

    let candidate_ports = [
        9197u16, 1990, 1443, 7676, 38400, 49152, 49153, 49154, 49155, 8008,
    ];
    let candidate_paths = [
        "/dmr.xml",
        "/description.xml",
        "/MediaRenderer/desc.xml",
        "/ssdp/device-desc.xml",
        "/dev/0/desc.xml",
    ];

    for port in candidate_ports {
        if !tcp_port_open(ip, port, Duration::from_millis(160)).await {
            continue;
        }
        for path in candidate_paths {
            let url = format!("http://{}:{}{}", ip, port, path);
            if let Ok(r) = http_request("GET", &url, &[], None, Duration::from_millis(700)).await
                && r.status < 400
                && let Some(parsed) = parse_upnp_device_xml(&url, &r.body_str())
                && let Some(av) = parsed.av_transport_control_url
            {
                return Some((av, parsed.connection_mgr_control_url));
            }
        }
    }

    None
}

pub struct DlnaSession {
    control_url: String,
    stopped: Arc<AtomicBool>,
    poll_task: Option<tokio::task::JoinHandle<()>>,
}

impl DlnaSession {
    pub async fn connect(ctx: CastContext) -> anyhow::Result<Self> {
        let mut av_ctrl = ctx.device.av_transport_control_url.clone();
        let mut cm_ctrl = ctx.device.connection_mgr_control_url.clone();

        if av_ctrl.is_none() {
            *ctx.state.write().await = CastingState::Connecting {
                protocol: "DLNA / UPnP".to_string(),
                message: format!("Mencari endpoint AVTransport di {}...", ctx.device.ip),
            };
            if let Some((found_av, found_cm)) = probe_dlna_control_urls(ctx.device.ip).await {
                av_ctrl = Some(found_av);
                cm_ctrl = found_cm;
            }
        }

        let control_url = av_ctrl.ok_or_else(|| {
            anyhow::anyhow!(
                "Perangkat {} tidak memiliki endpoint UPnP AVTransport:1",
                ctx.device.ip
            )
        })?;

        *ctx.state.write().await = CastingState::Connecting {
            protocol: "DLNA / UPnP".to_string(),
            message: format!("Menghubungi {} via UPnP AVTransport...", ctx.device.name),
        };

        // Query GetProtocolInfo when available to prioritize supported MIME
        // profiles.
        let mut sink_info = String::new();
        if let Some(cm) = cm_ctrl
            && let Ok(res) = send_soap_action(
                &cm,
                "urn:schemas-upnp-org:service:ConnectionManager:1",
                "GetProtocolInfo",
                "",
            )
            .await
        {
            sink_info = res.to_ascii_lowercase();
        }

        let _ = send_soap_action(
            &control_url,
            "urn:schemas-upnp-org:service:AVTransport:1",
            "Stop",
            "<InstanceID>0</InstanceID>",
        )
        .await;

        let mut profiles = vec![
            (
                "live.ts",
                "video/mp2t",
                "DLNA.ORG_PN=AVC_TS_MP_HD_AAC_MULT5;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
            (
                "live.mpeg",
                "video/mpeg",
                "DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
            (
                "live.mp4",
                "video/mp4",
                "DLNA.ORG_PN=AVC_MP4_MP_SD_AAC_MULT5;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000",
            ),
        ];

        if !sink_info.is_empty()
            && !sink_info.contains("video/mp2t")
            && sink_info.contains("video/mp4")
        {
            profiles.rotate_left(2);
        }

        let mut last_err = String::from("DLNA SetAVTransportURI gagal");
        let mut chosen_ok = false;

        for (path, mime, dlna_flags) in profiles {
            let stream_url = format!("http://{}:{}/{}", ctx.lan_ip, ctx.http_port, path);
            let didl = format!(
                r#"<DIDL-Lite xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/" xmlns:dc="http://purl.org/dc/elements/1.1/" xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"><item id="0" parentID="-1" restricted="1"><dc:title>NayutaCast Live Screen</dc:title><upnp:class>object.item.videoItem</upnp:class><res protocolInfo="http-get:*:{}:{}">{}</res></item></DIDL-Lite>"#,
                mime,
                dlna_flags,
                escape_xml(&stream_url)
            );
            let set_body = format!(
                "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
                escape_xml(&stream_url),
                escape_xml(&didl)
            );

            match send_soap_action(
                &control_url,
                "urn:schemas-upnp-org:service:AVTransport:1",
                "SetAVTransportURI",
                &set_body,
            )
            .await
            {
                Ok(_) => {
                    send_soap_action(
                        &control_url,
                        "urn:schemas-upnp-org:service:AVTransport:1",
                        "Play",
                        "<InstanceID>0</InstanceID><Speed>1</Speed>",
                    )
                    .await?;
                    chosen_ok = true;
                    break;
                }
                Err(e) => {
                    last_err = e.to_string();
                }
            }
        }

        if !chosen_ok {
            anyhow::bail!(last_err);
        }

        *ctx.state.write().await = CastingState::WaitingForReceiver {
            protocol: "DLNA Smart TV".to_string(),
            message: format!(
                "Menunggu {} menerima & menarik stream (periksa popup izin di layar TV jika muncul)...",
                ctx.device.name
            ),
            browser_url: ctx.browser_url(),
        };

        // Wait up to 8s verifying that TV actually starts pulling HTTP stream
        // or enters PLAYING.
        let mut verified = false;
        for _ in 0..32 {
            if ctx.hub.active_clients() > 0 && ctx.hub.bytes_served() > 0 {
                verified = true;
                break;
            }
            if let Ok(info_xml) = send_soap_action(
                &control_url,
                "urn:schemas-upnp-org:service:AVTransport:1",
                "GetTransportInfo",
                "<InstanceID>0</InstanceID>",
            )
            .await
                && info_xml.contains("PLAYING")
                && ctx.hub.bytes_served() > 0
            {
                verified = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }

        if !verified {
            let _ = send_soap_action(
                &control_url,
                "urn:schemas-upnp-org:service:AVTransport:1",
                "Stop",
                "<InstanceID>0</InstanceID>",
            )
            .await;
            anyhow::bail!(
                "TV {} menerima Play SOAP tetapi tidak menarik stream HTTP (timeout 8 detik)",
                ctx.device.name
            );
        }

        *ctx.state.write().await = CastingState::Connected {
            protocol: "DLNA Smart TV".to_string(),
            target_name: ctx.device.name.clone(),
            bytes_sent: ctx.hub.bytes_served(),
            active_clients: ctx.hub.active_clients().max(1),
            browser_url: ctx.browser_url(),
        };

        let stopped = Arc::new(AtomicBool::new(false));
        let stopped_bg = stopped.clone();
        let hub_bg: Arc<StreamHub> = ctx.hub.clone();
        let state_bg: Arc<RwLock<CastingState>> = ctx.state.clone();
        let ctrl_bg = control_url.clone();

        let poll_task = tokio::spawn(async move {
            while !stopped_bg.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_secs(2)).await;
                if stopped_bg.load(Ordering::SeqCst) {
                    break;
                }
                let clients = hub_bg.active_clients();
                let bytes = hub_bg.bytes_served();
                if let Ok(info) = send_soap_action(
                    &ctrl_bg,
                    "urn:schemas-upnp-org:service:AVTransport:1",
                    "GetTransportInfo",
                    "<InstanceID>0</InstanceID>",
                )
                .await
                    && info.contains("STOPPED")
                    && clients == 0
                {
                    break;
                }
                let mut st = state_bg.write().await;
                if let CastingState::Connected {
                    bytes_sent,
                    active_clients,
                    ..
                } = &mut *st
                {
                    *bytes_sent = bytes;
                    *active_clients = clients;
                }
            }
        });

        Ok(Self {
            control_url,
            stopped,
            poll_task: Some(poll_task),
        })
    }

    pub async fn stop(&mut self) {
        self.stopped.store(true, Ordering::SeqCst);
        if let Some(t) = self.poll_task.take() {
            t.abort();
        }
        let _ = send_soap_action(
            &self.control_url,
            "urn:schemas-upnp-org:service:AVTransport:1",
            "Stop",
            "<InstanceID>0</InstanceID>",
        )
        .await;
    }
}
