use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket as StdUdpSocket},
    sync::Arc,
    time::Duration,
};

use common::MediaStreamDescription;
use discovery::{DiscoveryContext, DiscoveryObserver, DiscoveryService};
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use tokio::net::UdpSocket;
use tokio::sync::{RwLock, Semaphore};
use url::Url;

use crate::sdk::net_utils::{
    self, LocalInterface, http_request, infer_network_kind_for_ip, list_active_ipv4_interfaces,
    tcp_port_open,
};

pub use net_utils::NetworkKind as DeviceKind;

pub const NAYUTACAST_APP_TOPIC: &str = "nayutacast-app-core";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CastProtocol {
    NayutacastNative,
    GoogleCast,
    DlnaAvTransport,
    AirPlay,
    RokuEcp,
    BrowserFallback,
}

impl CastProtocol {
    pub fn short_label(self) -> &'static str {
        match self {
            CastProtocol::NayutacastNative => "NayutaCast",
            CastProtocol::GoogleCast => "Google Cast",
            CastProtocol::DlnaAvTransport => "DLNA Smart TV",
            CastProtocol::AirPlay => "AirPlay",
            CastProtocol::RokuEcp => "Roku TV",
            CastProtocol::BrowserFallback => "Browser",
        }
    }

    pub fn from_setting_str(s: &str) -> Option<Self> {
        let low = s.to_ascii_lowercase();
        if low.contains("nayutacast") {
            Some(CastProtocol::NayutacastNative)
        } else if low.contains("cast") || low.contains("chromecast") {
            Some(CastProtocol::GoogleCast)
        } else if low.contains("dlna") || low.contains("upnp") {
            Some(CastProtocol::DlnaAvTransport)
        } else if low.contains("airplay") {
            Some(CastProtocol::AirPlay)
        } else if low.contains("roku") {
            Some(CastProtocol::RokuEcp)
        } else if low.contains("browser") {
            Some(CastProtocol::BrowserFallback)
        } else {
            None
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceType {
    Windows,
    Android,
    Apple,
    Linux,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServiceMessage {
    pub targets: Vec<String>,
    pub name: String,
    pub kind: DeviceType,
    #[serde(default)]
    pub description: Option<MediaStreamDescription>,
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub ip: IpAddr,
    pub port: u16,
    pub network: DeviceKind,
    pub manufacturer: Option<String>,
    pub model_name: Option<String>,
    pub protocols: Vec<CastProtocol>,
    pub av_transport_control_url: Option<String>,
    pub connection_mgr_control_url: Option<String>,
    pub location_url: Option<String>,
    pub cast_port: Option<u16>,
    pub airplay_port: Option<u16>,
    pub roku_port: Option<u16>,
    pub nayutacast_peer_id: Option<String>,
    pub is_custom: bool,
}

impl DeviceInfo {
    pub fn primary_protocol(&self, preferred: Option<CastProtocol>) -> CastProtocol {
        if let Some(pref) = preferred
            && self.protocols.contains(&pref)
        {
            return pref;
        }
        self.protocols
            .iter()
            .copied()
            .min()
            .unwrap_or(CastProtocol::BrowserFallback)
    }

    pub fn ordered_protocols(&self, preferred: Option<CastProtocol>) -> Vec<CastProtocol> {
        let mut list = self.protocols.clone();
        list.sort();
        list.dedup();
        if let Some(pref) = preferred
            && let Some(pos) = list.iter().position(|p| *p == pref)
        {
            let p = list.remove(pos);
            list.insert(0, p);
        }
        if !list.contains(&CastProtocol::BrowserFallback) {
            list.push(CastProtocol::BrowserFallback);
        }
        list
    }

    pub fn subtitle(&self, preferred: Option<CastProtocol>) -> String {
        let proto = self.primary_protocol(preferred).short_label();
        if let Some(model) = &self.model_name
            && !model.is_empty()
            && !self.name.contains(model)
        {
            format!("{} · {} ({})", self.ip, proto, model)
        } else {
            format!("{} · {}", self.ip, proto)
        }
    }

    fn merge_from(&mut self, incoming: DeviceInfo) {
        let is_generic = |n: &str| {
            n.starts_with("Smart TV (")
                || n.starts_with("Chromecast / Android TV")
                || n.starts_with("Apple TV / AirPlay")
                || n.starts_with("DLNA Media Receiver")
                || n.starts_with("Roku TV (")
                || n.starts_with("TV Manual")
                || n.starts_with("NayutaCast Receiver (")
        };

        if (!is_generic(&incoming.name) && is_generic(&self.name)) || incoming.is_custom {
            self.name = incoming.name;
        }
        if self.manufacturer.is_none() {
            self.manufacturer = incoming.manufacturer;
        }
        if self.model_name.is_none() {
            self.model_name = incoming.model_name;
        }
        if self.av_transport_control_url.is_none() {
            self.av_transport_control_url = incoming.av_transport_control_url;
        }
        if self.connection_mgr_control_url.is_none() {
            self.connection_mgr_control_url = incoming.connection_mgr_control_url;
        }
        if self.location_url.is_none() {
            self.location_url = incoming.location_url;
        }
        if self.cast_port.is_none() {
            self.cast_port = incoming.cast_port;
        }
        if self.airplay_port.is_none() {
            self.airplay_port = incoming.airplay_port;
        }
        if self.roku_port.is_none() {
            self.roku_port = incoming.roku_port;
        }
        if self.nayutacast_peer_id.is_none() {
            self.nayutacast_peer_id = incoming.nayutacast_peer_id;
        }
        if incoming.is_custom {
            self.is_custom = true;
            self.network = incoming.network;
        }
        for proto in incoming.protocols {
            if !self.protocols.contains(&proto) {
                self.protocols.push(proto);
            }
        }
        self.protocols.sort();
        self.port = match self.primary_protocol(None) {
            CastProtocol::NayutacastNative => 8080,
            CastProtocol::GoogleCast => self.cast_port.unwrap_or(8009),
            CastProtocol::DlnaAvTransport => {
                if self.port != 0 && self.port != 8080 {
                    self.port
                } else {
                    1900
                }
            }
            CastProtocol::AirPlay => self.airplay_port.unwrap_or(7000),
            CastProtocol::RokuEcp => self.roku_port.unwrap_or(8060),
            CastProtocol::BrowserFallback => 80,
        };
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedUpnpDevice {
    pub friendly_name: String,
    pub manufacturer: Option<String>,
    pub model_name: Option<String>,
    pub protocols: Vec<CastProtocol>,
    pub av_transport_control_url: Option<String>,
    pub connection_mgr_control_url: Option<String>,
    pub has_dial: bool,
}

/// Parses a UPnP/SSDP `device-desc.xml` document and filters out
/// routers/modems/gateways.
pub fn parse_upnp_device_xml(location_url: &str, xml_body: &str) -> Option<ParsedUpnpDevice> {
    let doc = roxmltree::Document::parse(xml_body).ok()?;
    let base_url = Url::parse(location_url).ok()?;
    let url_base = doc
        .descendants()
        .find(|n| n.tag_name().name().eq_ignore_ascii_case("URLBase"))
        .and_then(|n| n.text())
        .and_then(|t| Url::parse(t.trim()).ok())
        .unwrap_or(base_url);

    let first_text = |tag: &str| -> Option<String> {
        doc.descendants()
            .find(|n| n.tag_name().name().eq_ignore_ascii_case(tag))
            .and_then(|n| n.text())
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    };

    let device_type = first_text("deviceType").unwrap_or_default();
    let device_type_low = device_type.to_ascii_lowercase();
    if device_type_low.contains("internetgatewaydevice")
        || device_type_low.contains("wandevice")
        || device_type_low.contains("wfawlandev")
        || device_type_low.contains("printer")
    {
        return None;
    }

    let friendly_name = first_text("friendlyName").unwrap_or_else(|| "Smart TV".to_string());
    let manufacturer = first_text("manufacturer");
    let model_name = first_text("modelName");

    let mut protocols = Vec::new();
    let mut av_transport_control_url = None;
    let mut connection_mgr_control_url = None;
    let mut has_dial = false;

    for svc in doc
        .descendants()
        .filter(|n| n.tag_name().name().eq_ignore_ascii_case("service"))
    {
        let svc_type = svc
            .children()
            .find(|c| c.tag_name().name().eq_ignore_ascii_case("serviceType"))
            .and_then(|c| c.text())
            .unwrap_or("");
        let ctrl_url_raw = svc
            .children()
            .find(|c| c.tag_name().name().eq_ignore_ascii_case("controlURL"))
            .and_then(|c| c.text())
            .map(str::trim);

        let svc_low = svc_type.to_ascii_lowercase();
        if svc_low.contains("avtransport") {
            if let Some(raw) = ctrl_url_raw
                && let Ok(joined) = url_base.join(raw)
            {
                av_transport_control_url = Some(joined.to_string());
                if !protocols.contains(&CastProtocol::DlnaAvTransport) {
                    protocols.push(CastProtocol::DlnaAvTransport);
                }
            }
        } else if svc_low.contains("connectionmanager") {
            if let Some(raw) = ctrl_url_raw
                && let Ok(joined) = url_base.join(raw)
            {
                connection_mgr_control_url = Some(joined.to_string());
            }
        } else if svc_low.contains("dial-multiscreen") {
            has_dial = true;
        } else if svc_low.contains("ecp") && !protocols.contains(&CastProtocol::RokuEcp) {
            protocols.push(CastProtocol::RokuEcp);
        }
    }

    if let Some(mfg) = &manufacturer
        && mfg.to_ascii_lowercase().contains("roku")
        && !protocols.contains(&CastProtocol::RokuEcp)
    {
        protocols.push(CastProtocol::RokuEcp);
    }

    if has_dial
        && manufacturer
            .as_deref()
            .unwrap_or("")
            .to_ascii_lowercase()
            .contains("google")
        && !protocols.contains(&CastProtocol::GoogleCast)
    {
        protocols.push(CastProtocol::GoogleCast);
    }

    if protocols.is_empty() && !has_dial {
        return None;
    }

    Some(ParsedUpnpDevice {
        friendly_name,
        manufacturer,
        model_name,
        protocols,
        av_transport_control_url,
        connection_mgr_control_url,
        has_dial,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedMdnsService {
    pub service_type: String,
    pub instance_name: String,
    pub target_host: String,
    pub ip: Option<Ipv4Addr>,
    pub port: u16,
    pub friendly_name: Option<String>,
    pub model: Option<String>,
    pub protocol: CastProtocol,
}

/// Decodes raw mDNS wire response bytes (`_googlecast._tcp.local.` &
/// `_airplay._tcp.local.`).
pub fn parse_mdns_response(pkt: &[u8]) -> Vec<ParsedMdnsService> {
    if pkt.len() < 12 {
        return Vec::new();
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let ancount = u16::from_be_bytes([pkt[6], pkt[7]]) as usize;
    let nscount = u16::from_be_bytes([pkt[8], pkt[9]]) as usize;
    let arcount = u16::from_be_bytes([pkt[10], pkt[11]]) as usize;

    let mut pos = 12usize;
    for _ in 0..qdcount {
        let Some((_, next_pos)) = read_dns_name(pkt, pos) else {
            return Vec::new();
        };
        pos = next_pos + 4;
        if pos > pkt.len() {
            return Vec::new();
        }
    }

    let mut ptrs: Vec<(String, String)> = Vec::new();
    let mut srvs: HashMap<String, (u16, String)> = HashMap::new();
    let mut txts: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut a_recs: HashMap<String, Ipv4Addr> = HashMap::new();

    let total_rrs = ancount + nscount + arcount;
    for _ in 0..total_rrs {
        let Some((rr_name, after_name)) = read_dns_name(pkt, pos) else {
            break;
        };
        if after_name + 10 > pkt.len() {
            break;
        }
        let rtype = u16::from_be_bytes([pkt[after_name], pkt[after_name + 1]]);
        let rdlength = u16::from_be_bytes([pkt[after_name + 8], pkt[after_name + 9]]) as usize;
        let rdata_start = after_name + 10;
        let rdata_end = rdata_start + rdlength;
        if rdata_end > pkt.len() {
            break;
        }
        let rdata = &pkt[rdata_start..rdata_end];

        match rtype {
            12 => {
                // PTR
                if let Some((target, _)) = read_dns_name(pkt, rdata_start) {
                    ptrs.push((rr_name.to_ascii_lowercase(), target));
                }
            }
            33 => {
                // SRV
                if rdata.len() >= 6
                    && let Some((target, _)) = read_dns_name(pkt, rdata_start + 6)
                {
                    let port = u16::from_be_bytes([rdata[4], rdata[5]]);
                    srvs.insert(rr_name.to_ascii_lowercase(), (port, target));
                }
            }
            16 => {
                // TXT
                let mut map = HashMap::new();
                let mut tpos = 0usize;
                while tpos < rdata.len() {
                    let len = rdata[tpos] as usize;
                    tpos += 1;
                    if tpos + len > rdata.len() {
                        break;
                    }
                    let kv = String::from_utf8_lossy(&rdata[tpos..tpos + len]);
                    if let Some((k, v)) = kv.split_once('=') {
                        map.insert(k.to_string(), v.to_string());
                    }
                    tpos += len;
                }
                txts.insert(rr_name.to_ascii_lowercase(), map);
            }
            1 if rdata.len() == 4 => {
                // A
                a_recs.insert(
                    rr_name.to_ascii_lowercase(),
                    Ipv4Addr::new(rdata[0], rdata[1], rdata[2], rdata[3]),
                );
            }
            _ => {}
        }

        pos = rdata_end;
    }

    let mut out = Vec::new();
    let candidates: Vec<(String, String)> = if !ptrs.is_empty() {
        ptrs
    } else {
        srvs.keys()
            .map(|inst| {
                let st = if inst.contains("_googlecast") {
                    "_googlecast._tcp.local".to_string()
                } else {
                    "_airplay._tcp.local".to_string()
                };
                (st, inst.clone())
            })
            .collect()
    };

    for (svc_type, inst) in candidates {
        let inst_low = inst.to_ascii_lowercase();
        let protocol = if svc_type.contains("_googlecast") || inst_low.contains("_googlecast") {
            CastProtocol::GoogleCast
        } else if svc_type.contains("_airplay") || inst_low.contains("_airplay") {
            CastProtocol::AirPlay
        } else {
            continue;
        };

        let Some((port, target_host)) = srvs.get(&inst_low).cloned() else {
            continue;
        };
        let ip = a_recs.get(&target_host.to_ascii_lowercase()).copied();
        let txt = txts.get(&inst_low);
        let friendly_name = txt
            .and_then(|m| m.get("fn").cloned())
            .or_else(|| Some(inst.split('.').next().unwrap_or(&inst).to_string()));
        let model = txt.and_then(|m| m.get("md").or_else(|| m.get("model")).cloned());

        out.push(ParsedMdnsService {
            service_type: svc_type,
            instance_name: inst,
            target_host,
            ip,
            port,
            friendly_name,
            model,
            protocol,
        });
    }

    out
}

fn read_dns_name(pkt: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut pos = start;
    let mut end_pos = None;
    let mut jumps = 0u8;

    loop {
        if pos >= pkt.len() || jumps > 16 {
            return None;
        }
        let len = pkt[pos];
        if len == 0 {
            if end_pos.is_none() {
                end_pos = Some(pos + 1);
            }
            break;
        }
        if (len & 0xC0) == 0xC0 {
            if pos + 1 >= pkt.len() {
                return None;
            }
            let ptr = (((len as usize) & 0x3F) << 8) | (pkt[pos + 1] as usize);
            if end_pos.is_none() {
                end_pos = Some(pos + 2);
            }
            pos = ptr;
            jumps += 1;
            continue;
        }
        let llen = len as usize;
        pos += 1;
        if pos + llen > pkt.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&pkt[pos..pos + llen]).to_string());
        pos += llen;
    }

    Some((labels.join("."), end_pos.unwrap_or(pos)))
}

fn build_mdns_ptr_query(service_names: &[&str]) -> Vec<u8> {
    let mut pkt = vec![
        0x00,
        0x00, // ID = 0
        0x00,
        0x00, // Standard Query
        0x00,
        service_names.len() as u8, // QDCOUNT
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
        0x00,
    ];
    for name in service_names {
        for label in name.trim_end_matches('.').split('.') {
            let bytes = label.as_bytes();
            pkt.push(bytes.len() as u8);
            pkt.extend_from_slice(bytes);
        }
        pkt.push(0x00);
        pkt.extend_from_slice(&12u16.to_be_bytes()); // QTYPE = PTR
        pkt.extend_from_slice(&0x8001u16.to_be_bytes()); // QCLASS = IN | QU (unicast response requested)
    }
    pkt
}

#[derive(Clone)]
struct AppDiscoveryObserver {
    devices: Arc<RwLock<HashMap<String, DeviceInfo>>>,
    local_name: Arc<RwLock<String>>,
    ifaces_snapshot: Arc<RwLock<Vec<LocalInterface>>>,
}

impl DiscoveryObserver for AppDiscoveryObserver {
    async fn online(&self, mut ctx: DiscoveryContext<'_>) {
        info!("NayutaCast peer online: {} ({})", ctx.id, ctx.ip);
        let ifaces = self.ifaces_snapshot.read().await;
        let ip = IpAddr::V4(ctx.ip);
        let net = infer_network_kind_for_ip(ip, &ifaces);
        let dev = DeviceInfo {
            id: format!("ip:{}", ctx.ip),
            name: format!("NayutaCast Receiver ({})", ctx.ip),
            ip,
            port: 8080,
            network: net,
            manufacturer: Some("NayutaCast".into()),
            model_name: Some("Native Receiver".into()),
            protocols: vec![CastProtocol::NayutacastNative],
            av_transport_control_url: None,
            connection_mgr_control_url: None,
            location_url: None,
            cast_port: None,
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: Some(ctx.id.clone()),
            is_custom: false,
        };
        DeviceManager::merge_into_map(&self.devices, dev).await;

        let name = self.local_name.read().await.clone();
        let hello = ServiceMessage {
            targets: vec![ctx.id.clone()],
            name,
            kind: DeviceType::Linux,
            description: None,
        };
        if let Ok(bytes) = serde_json::to_vec(&hello) {
            let _ = ctx.broadcast(bytes);
        }
    }

    async fn offline(&self, ctx: DiscoveryContext<'_>) {
        info!("NayutaCast peer offline: {} ({})", ctx.id, ctx.ip);
        let mut map = self.devices.write().await;
        let key = format!("ip:{}", ctx.ip);
        if let Some(dev) = map.get_mut(&key) {
            dev.nayutacast_peer_id = None;
            dev.protocols
                .retain(|p| *p != CastProtocol::NayutacastNative);
            if dev.protocols.is_empty() && !dev.is_custom {
                map.remove(&key);
            }
        }
    }

    async fn on_message(&self, ctx: DiscoveryContext<'_>, message: Vec<u8>) {
        let Ok(msg) = serde_json::from_slice::<ServiceMessage>(&message) else {
            return;
        };
        let ip = IpAddr::V4(ctx.ip);
        let ifaces = self.ifaces_snapshot.read().await;
        let net = infer_network_kind_for_ip(ip, &ifaces);
        let kind_label = format!("{:?}", msg.kind);
        let dev = DeviceInfo {
            id: format!("ip:{}", ctx.ip),
            name: msg.name,
            ip,
            port: 8080,
            network: net,
            manufacturer: Some("NayutaCast".into()),
            model_name: Some(format!("{} Receiver", kind_label)),
            protocols: vec![CastProtocol::NayutacastNative],
            av_transport_control_url: None,
            connection_mgr_control_url: None,
            location_url: None,
            cast_port: None,
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: Some(ctx.id),
            is_custom: false,
        };
        DeviceManager::merge_into_map(&self.devices, dev).await;
    }
}

pub struct DeviceManager {
    pub devices: Arc<RwLock<HashMap<String, DeviceInfo>>>,
    pub scanning: Arc<RwLock<bool>>,
    service: Arc<RwLock<Option<Arc<DiscoveryService>>>>,
    local_name: Arc<RwLock<String>>,
    ifaces_snapshot: Arc<RwLock<Vec<LocalInterface>>>,
}

impl DeviceManager {
    pub fn new(device_name: &str) -> Self {
        Self {
            devices: Arc::new(RwLock::new(HashMap::new())),
            scanning: Arc::new(RwLock::new(false)),
            service: Arc::new(RwLock::new(None)),
            local_name: Arc::new(RwLock::new(device_name.to_string())),
            ifaces_snapshot: Arc::new(RwLock::new(list_active_ipv4_interfaces())),
        }
    }

    async fn merge_into_map(map_lock: &Arc<RwLock<HashMap<String, DeviceInfo>>>, dev: DeviceInfo) {
        let mut map = map_lock.write().await;
        if let Some(existing) = map.get_mut(&dev.id) {
            existing.merge_from(dev);
        } else {
            map.insert(dev.id.clone(), dev);
        }
    }

    pub async fn set_local_name(&self, name: &str) {
        *self.local_name.write().await = name.to_string();
    }

    /// Initializes the persistent libp2p NayutaCast discovery service on
    /// `nayutacast-app-core`.
    pub async fn ensure_p2p_started(&self) {
        if self.service.read().await.is_some() {
            return;
        }
        let observer = AppDiscoveryObserver {
            devices: self.devices.clone(),
            local_name: self.local_name.clone(),
            ifaces_snapshot: self.ifaces_snapshot.clone(),
        };
        match DiscoveryService::new(NAYUTACAST_APP_TOPIC.to_string(), observer).await {
            Ok(svc) => {
                *self.service.write().await = Some(Arc::new(svc));
                info!(
                    "libp2p DiscoveryService initialized on topic '{}'",
                    NAYUTACAST_APP_TOPIC
                );
            }
            Err(e) => warn!("libp2p DiscoveryService unavailable: {}", e),
        }
    }

    /// Broadcasts a `ServiceMessage` over Gossipsub so NayutaCast receivers can
    /// connect to our SRT stream.
    pub async fn announce_stream(
        &self,
        target_peer_id: Option<&str>,
        sender_name: &str,
        description: Option<MediaStreamDescription>,
    ) -> anyhow::Result<()> {
        let svc = self
            .service
            .read()
            .await
            .clone()
            .ok_or_else(|| anyhow::anyhow!("DiscoveryService is not initialized"))?;

        let msg = ServiceMessage {
            targets: target_peer_id
                .map(|id| vec![id.to_string()])
                .unwrap_or_default(),
            name: sender_name.to_string(),
            kind: DeviceType::Linux,
            description,
        };
        let payload = serde_json::to_vec(&msg)?;
        svc.broadcast(payload)
            .await
            .map_err(|e| anyhow::anyhow!("Gossipsub broadcast error: {}", e))?;
        Ok(())
    }

    /// Checks whether a given network kind (`Wifi` / `Lan`) has at least one
    /// active interface.
    #[allow(dead_code)]
    pub async fn has_active_interface(&self, kind: DeviceKind) -> bool {
        self.ifaces_snapshot
            .read()
            .await
            .iter()
            .any(|i| i.kind == kind)
    }

    pub async fn start_scan(self: Arc<Self>) {
        if *self.scanning.read().await {
            return;
        }
        *self.scanning.write().await = true;

        let ifaces = list_active_ipv4_interfaces();
        *self.ifaces_snapshot.write().await = ifaces.clone();
        self.ensure_p2p_started().await;

        {
            let mut map = self.devices.write().await;
            map.retain(|_, d| d.is_custom || d.nayutacast_peer_id.is_some());
        }

        let d_ssdp = self.devices.clone();
        let d_mdns = self.devices.clone();
        let d_probe = self.devices.clone();
        let if_ssdp = ifaces.clone();
        let if_mdns = ifaces.clone();
        let if_probe = ifaces;

        let (_, _, _) = tokio::join!(
            Self::scan_ssdp_multi_nic(d_ssdp, if_ssdp),
            Self::scan_mdns_multi_nic(d_mdns, if_mdns),
            Self::scan_arp_and_subnet(d_probe, if_probe),
        );

        *self.scanning.write().await = false;
    }

    async fn scan_ssdp_multi_nic(
        devices: Arc<RwLock<HashMap<String, DeviceInfo>>>,
        ifaces: Vec<LocalInterface>,
    ) {
        let targets = [
            "urn:schemas-upnp-org:device:MediaRenderer:1",
            "urn:schemas-upnp-org:service:AVTransport:1",
            "urn:dial-multiscreen-org:service:dial:1",
            "roku:ecp",
        ];

        let mut seen_locations = HashSet::new();
        let mut raw_hits: Vec<(IpAddr, String, String)> = Vec::new();

        for iface in &ifaces {
            let bind_addr = SocketAddr::from((iface.ip, 0));
            let Ok(std_sock) = StdUdpSocket::bind(bind_addr) else {
                continue;
            };
            let _ = std_sock.set_nonblocking(true);
            let Ok(sock) = UdpSocket::from_std(std_sock) else {
                continue;
            };

            let ssdp_dest = SocketAddr::from((Ipv4Addr::new(239, 255, 255, 250), 1900));
            for st in targets {
                let req = format!(
                    "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {}\r\n\r\n",
                    st
                );
                let _ = sock.send_to(req.as_bytes(), ssdp_dest).await;
            }

            let mut buf = [0u8; 4096];
            let deadline = tokio::time::Instant::now() + Duration::from_millis(1600);
            while tokio::time::Instant::now() < deadline {
                let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(remain, sock.recv_from(&mut buf)).await {
                    Ok(Ok((len, addr))) => {
                        let resp = String::from_utf8_lossy(&buf[..len]);
                        let loc = resp
                            .lines()
                            .find(|l| l.len() > 9 && l[..9].eq_ignore_ascii_case("location:"))
                            .map(|l| l[9..].trim().to_string());
                        if let Some(location) = loc
                            && seen_locations.insert(location.clone())
                        {
                            raw_hits.push((addr.ip(), location, resp.into_owned()));
                        }
                    }
                    _ => break,
                }
            }
        }

        let tasks = raw_hits.into_iter().map(|(ip, loc, ssdp_hdr)| {
            let devices = devices.clone();
            let ifaces = ifaces.clone();
            async move {
                Self::verify_and_store_upnp(&devices, ip, &loc, &ssdp_hdr, &ifaces, false).await;
            }
        });
        for t in tasks {
            t.await;
        }
    }

    async fn verify_and_store_upnp(
        devices: &Arc<RwLock<HashMap<String, DeviceInfo>>>,
        ip: IpAddr,
        location_url: &str,
        ssdp_hdr: &str,
        ifaces: &[LocalInterface],
        is_custom: bool,
    ) -> bool {
        let Ok(resp) =
            http_request("GET", location_url, &[], None, Duration::from_millis(1200)).await
        else {
            return false;
        };
        if resp.status >= 400 || resp.body.is_empty() {
            return false;
        }
        let xml_str = resp.body_str();
        let Some(parsed) = parse_upnp_device_xml(location_url, &xml_str) else {
            debug!("Filtered out non-TV UPnP device at {}", location_url);
            return false;
        };

        let mut protocols = parsed.protocols;
        let cast_open = if parsed.has_dial || ssdp_hdr.to_ascii_lowercase().contains("dial") {
            tcp_port_open(ip, 8009, Duration::from_millis(200)).await
        } else {
            false
        };
        if cast_open && !protocols.contains(&CastProtocol::GoogleCast) {
            protocols.push(CastProtocol::GoogleCast);
        }
        if protocols.is_empty() {
            return false;
        }

        let port = Url::parse(location_url)
            .ok()
            .and_then(|u| u.port_or_known_default())
            .unwrap_or(1900);
        let net = infer_network_kind_for_ip(ip, ifaces);

        let dev = DeviceInfo {
            id: format!("ip:{}", ip),
            name: parsed.friendly_name,
            ip,
            port,
            network: net,
            manufacturer: parsed.manufacturer,
            model_name: parsed.model_name,
            protocols,
            av_transport_control_url: parsed.av_transport_control_url,
            connection_mgr_control_url: parsed.connection_mgr_control_url,
            location_url: Some(location_url.to_string()),
            cast_port: cast_open.then_some(8009),
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: None,
            is_custom,
        };
        Self::merge_into_map(devices, dev).await;
        true
    }

    async fn scan_mdns_multi_nic(
        devices: Arc<RwLock<HashMap<String, DeviceInfo>>>,
        ifaces: Vec<LocalInterface>,
    ) {
        let query = build_mdns_ptr_query(&["_googlecast._tcp.local", "_airplay._tcp.local"]);
        let mdns_dest = SocketAddr::from((Ipv4Addr::new(224, 0, 0, 251), 5353));

        for iface in &ifaces {
            let Ok(std_sock) = StdUdpSocket::bind(SocketAddr::from((iface.ip, 0))) else {
                continue;
            };
            let _ = std_sock.set_nonblocking(true);
            let Ok(sock) = UdpSocket::from_std(std_sock) else {
                continue;
            };

            let _ = sock.send_to(&query, mdns_dest).await;
            tokio::time::sleep(Duration::from_millis(60)).await;
            let _ = sock.send_to(&query, mdns_dest).await;

            let mut buf = [0u8; 4096];
            let deadline = tokio::time::Instant::now() + Duration::from_millis(1200);
            while tokio::time::Instant::now() < deadline {
                let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
                match tokio::time::timeout(remain, sock.recv_from(&mut buf)).await {
                    Ok(Ok((len, src_addr))) => {
                        for svc in parse_mdns_response(&buf[..len]) {
                            let ip = svc.ip.map(IpAddr::V4).unwrap_or_else(|| src_addr.ip());
                            let net = infer_network_kind_for_ip(ip, &ifaces);
                            let dev = DeviceInfo {
                                id: format!("ip:{}", ip),
                                name: svc
                                    .friendly_name
                                    .unwrap_or_else(|| format!("Smart TV ({})", ip)),
                                ip,
                                port: svc.port,
                                network: net,
                                manufacturer: match svc.protocol {
                                    CastProtocol::GoogleCast => Some("Google / Android TV".into()),
                                    CastProtocol::AirPlay => Some("Apple / AirPlay".into()),
                                    _ => None,
                                },
                                model_name: svc.model,
                                protocols: vec![svc.protocol],
                                av_transport_control_url: None,
                                connection_mgr_control_url: None,
                                location_url: None,
                                cast_port: (svc.protocol == CastProtocol::GoogleCast)
                                    .then_some(svc.port),
                                airplay_port: (svc.protocol == CastProtocol::AirPlay)
                                    .then_some(svc.port),
                                roku_port: None,
                                nayutacast_peer_id: None,
                                is_custom: false,
                            };
                            Self::merge_into_map(&devices, dev).await;
                        }
                    }
                    _ => break,
                }
            }
        }
    }

    async fn scan_arp_and_subnet(
        devices: Arc<RwLock<HashMap<String, DeviceInfo>>>,
        ifaces: Vec<LocalInterface>,
    ) {
        let mut candidate_ips: HashSet<IpAddr> = HashSet::new();

        if let Ok(arp) = tokio::fs::read_to_string("/proc/net/arp").await {
            for line in arp.lines().skip(1) {
                if let Some(ip_str) = line.split_whitespace().next()
                    && let Ok(ip) = ip_str.parse::<IpAddr>()
                    && !ip.is_loopback()
                    && !ip.is_unspecified()
                {
                    candidate_ips.insert(ip);
                }
            }
        }

        // Add bounded /24 neighbors for active Wi-Fi/LAN subnets so asleep TVs
        // with empty ARP are still found.
        for iface in &ifaces {
            let octets = iface.ip.octets();
            if octets[0] == 172 && (16..=31).contains(&octets[1]) {
                continue; // skip private docker/bridge-like ranges if any remained
            }
            for host in 2u8..=254u8 {
                let ip = Ipv4Addr::new(octets[0], octets[1], octets[2], host);
                if ip != iface.ip {
                    candidate_ips.insert(IpAddr::V4(ip));
                }
            }
        }

        let sem = Arc::new(Semaphore::new(64));
        let mut handles = Vec::with_capacity(candidate_ips.len());
        for ip in candidate_ips {
            let sem = sem.clone();
            let devices = devices.clone();
            let ifaces = ifaces.clone();
            handles.push(tokio::spawn(async move {
                let Ok(_permit) = sem.acquire().await else {
                    return;
                };
                Self::probe_single_ip(&devices, ip, None, None, None, &ifaces, false).await;
            }));
        }

        for h in handles {
            let _ = h.await;
        }
    }

    async fn probe_single_ip(
        devices: &Arc<RwLock<HashMap<String, DeviceInfo>>>,
        ip: IpAddr,
        custom_name: Option<String>,
        protocol_override: Option<CastProtocol>,
        network_override: Option<DeviceKind>,
        ifaces: &[LocalInterface],
        is_custom: bool,
    ) -> Option<DeviceInfo> {
        let timeout = if is_custom {
            Duration::from_millis(600)
        } else {
            Duration::from_millis(140)
        };

        let (p8009, p8008, p7000, p8060, p8080, p9197) = tokio::join!(
            tcp_port_open(ip, 8009, timeout),
            tcp_port_open(ip, 8008, timeout),
            tcp_port_open(ip, 7000, timeout),
            tcp_port_open(ip, 8060, timeout),
            tcp_port_open(ip, 8080, timeout),
            tcp_port_open(ip, 9197, timeout),
        );

        let any_open = p8009 || p8008 || p7000 || p8060 || p8080 || p9197;
        if !any_open && !is_custom {
            return None;
        }

        let net = network_override.unwrap_or_else(|| infer_network_kind_for_ip(ip, ifaces));
        let mut protocols = Vec::new();
        let mut friendly_name = custom_name.clone();
        let mut manufacturer = None;
        let mut model_name = None;
        let mut av_ctrl = None;
        let mut cm_ctrl = None;
        let mut location_url = None;

        if p8008 {
            let url = format!("http://{}:8008/ssdp/device-desc.xml", ip);
            if let Ok(r) = http_request("GET", &url, &[], None, Duration::from_millis(700)).await
                && r.status < 400
                && let Some(parsed) = parse_upnp_device_xml(&url, &r.body_str())
            {
                if friendly_name.is_none() {
                    friendly_name = Some(parsed.friendly_name);
                }
                manufacturer = parsed.manufacturer;
                model_name = parsed.model_name;
                av_ctrl = parsed.av_transport_control_url;
                cm_ctrl = parsed.connection_mgr_control_url;
                location_url = Some(url);
                for p in parsed.protocols {
                    if !protocols.contains(&p) {
                        protocols.push(p);
                    }
                }
            }
        }

        if p8009 && !protocols.contains(&CastProtocol::GoogleCast) {
            protocols.push(CastProtocol::GoogleCast);
        }
        if p7000 && !protocols.contains(&CastProtocol::AirPlay) {
            protocols.push(CastProtocol::AirPlay);
        }
        if p8060 {
            let roku_url = format!("http://{}:8060/query/device-info", ip);
            if let Ok(r) =
                http_request("GET", &roku_url, &[], None, Duration::from_millis(600)).await
                && r.status < 400
            {
                let xml = r.body_str();
                if let Ok(doc) = roxmltree::Document::parse(&xml) {
                    if friendly_name.is_none() {
                        friendly_name = doc
                            .descendants()
                            .find(|n| {
                                n.tag_name().name().eq_ignore_ascii_case("user-device-name")
                                    || n.tag_name()
                                        .name()
                                        .eq_ignore_ascii_case("friendly-device-name")
                            })
                            .and_then(|n| n.text())
                            .map(|s| s.trim().to_string());
                    }
                    model_name = model_name.or_else(|| {
                        doc.descendants()
                            .find(|n| n.tag_name().name().eq_ignore_ascii_case("model-name"))
                            .and_then(|n| n.text())
                            .map(|s| s.trim().to_string())
                    });
                    manufacturer = manufacturer.or_else(|| Some("Roku".into()));
                }
                protocols.push(CastProtocol::RokuEcp);
            }
        }
        if p9197 && !protocols.contains(&CastProtocol::DlnaAvTransport) {
            protocols.push(CastProtocol::DlnaAvTransport);
        }
        if p8080 && !protocols.contains(&CastProtocol::NayutacastNative) {
            // NayutaCast listener or HTTP renderer; only add Native if P2P
            // already confirmed OR explicit override
            if protocol_override == Some(CastProtocol::NayutacastNative) {
                protocols.push(CastProtocol::NayutacastNative);
            }
        }

        // Unicast SSDP discovery fallback when custom IP is probed or DLNA port
        // is hidden on ephemeral port.
        if is_custom
            && av_ctrl.is_none()
            && let Some(dlna) = Self::unicast_ssdp_probe(ip).await
        {
            if friendly_name.is_none() {
                friendly_name = Some(dlna.friendly_name);
            }
            manufacturer = manufacturer.or(dlna.manufacturer);
            model_name = model_name.or(dlna.model_name);
            av_ctrl = dlna.av_transport_control_url;
            cm_ctrl = dlna.connection_mgr_control_url;
            for p in dlna.protocols {
                if !protocols.contains(&p) {
                    protocols.push(p);
                }
            }
        }

        if let Some(pref) = protocol_override
            && !protocols.contains(&pref)
        {
            protocols.insert(0, pref);
        }

        if protocols.is_empty() {
            if !is_custom {
                return None;
            }
            protocols.push(CastProtocol::BrowserFallback);
        }

        protocols.sort();
        let name = friendly_name.unwrap_or_else(|| {
            if protocols.contains(&CastProtocol::GoogleCast) {
                format!("Chromecast / Android TV ({})", ip)
            } else if protocols.contains(&CastProtocol::DlnaAvTransport) {
                format!("DLNA Smart TV ({})", ip)
            } else if protocols.contains(&CastProtocol::AirPlay) {
                format!("Apple TV / AirPlay ({})", ip)
            } else if protocols.contains(&CastProtocol::RokuEcp) {
                format!("Roku TV ({})", ip)
            } else {
                format!("Smart TV ({})", ip)
            }
        });

        let dev = DeviceInfo {
            id: format!("ip:{}", ip),
            name,
            ip,
            port: if p8009 {
                8009
            } else if p7000 {
                7000
            } else if p8060 {
                8060
            } else {
                1900
            },
            network: net,
            manufacturer,
            model_name,
            protocols,
            av_transport_control_url: av_ctrl,
            connection_mgr_control_url: cm_ctrl,
            location_url,
            cast_port: p8009.then_some(8009),
            airplay_port: p7000.then_some(7000),
            roku_port: p8060.then_some(8060),
            nayutacast_peer_id: None,
            is_custom,
        };
        Self::merge_into_map(devices, dev.clone()).await;
        Some(dev)
    }

    pub async fn unicast_ssdp_probe(ip: IpAddr) -> Option<ParsedUpnpDevice> {
        let sock = UdpSocket::bind("0.0.0.0:0").await.ok()?;
        let req = "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: urn:schemas-upnp-org:device:MediaRenderer:1\r\n\r\n";
        let _ = sock
            .send_to(req.as_bytes(), SocketAddr::new(ip, 1900))
            .await;
        let mut buf = [0u8; 4096];
        if let Ok(Ok((len, _))) =
            tokio::time::timeout(Duration::from_millis(800), sock.recv_from(&mut buf)).await
        {
            let resp = String::from_utf8_lossy(&buf[..len]);
            if let Some(loc) = resp
                .lines()
                .find(|l| l.len() > 9 && l[..9].eq_ignore_ascii_case("location:"))
                .map(|l| l[9..].trim())
                && let Ok(http_res) =
                    http_request("GET", loc, &[], None, Duration::from_millis(1000)).await
                && let Some(parsed) = parse_upnp_device_xml(loc, &http_res.body_str())
            {
                return Some(parsed);
            }
        }
        None
    }

    /// Probes a user-entered IP and registers it immediately with full protocol
    /// metadata.
    pub async fn probe_and_add_custom(
        &self,
        ip: IpAddr,
        custom_name: Option<String>,
        protocol_override: Option<CastProtocol>,
        kind: DeviceKind,
    ) -> DeviceInfo {
        let ifaces = self.ifaces_snapshot.read().await.clone();
        Self::probe_single_ip(
            &self.devices,
            ip,
            custom_name,
            protocol_override,
            Some(kind),
            &ifaces,
            true,
        )
        .await
        .unwrap_or_else(|| DeviceInfo {
            id: format!("ip:{}", ip),
            name: format!("Smart TV ({})", ip),
            ip,
            port: 8009,
            network: kind,
            manufacturer: None,
            model_name: None,
            protocols: vec![protocol_override.unwrap_or(CastProtocol::BrowserFallback)],
            av_transport_control_url: None,
            connection_mgr_control_url: None,
            location_url: None,
            cast_port: None,
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: None,
            is_custom: true,
        })
    }

    #[allow(dead_code)]
    pub async fn add_custom_device(&self, ip: IpAddr, kind: DeviceKind) {
        let _ = self.probe_and_add_custom(ip, None, None, kind).await;
    }

    #[allow(dead_code)]
    pub async fn get_devices(&self) -> Vec<DeviceInfo> {
        let mut list: Vec<DeviceInfo> = self.devices.read().await.values().cloned().collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        list
    }

    /// Non-blocking UI snapshot helpers used by `eframe::App::ui` on every
    /// frame.
    pub fn list_nonblocking(&self, kind: DeviceKind) -> Option<Vec<DeviceInfo>> {
        let guard = self.devices.try_read().ok()?;
        let mut list: Vec<DeviceInfo> = guard
            .values()
            .filter(|d| d.network == kind)
            .cloned()
            .collect();
        list.sort_by(|a, b| a.name.cmp(&b.name));
        Some(list)
    }

    pub fn is_scanning_nonblocking(&self) -> Option<bool> {
        self.scanning.try_read().ok().map(|g| *g)
    }

    pub fn has_active_interface_nonblocking(&self, kind: DeviceKind) -> Option<bool> {
        self.ifaces_snapshot
            .try_read()
            .ok()
            .map(|g| g.iter().any(|i| i.kind == kind))
    }

    pub fn trigger_scan(self: &Arc<Self>, rt: &tokio::runtime::Handle) {
        let this = self.clone();
        rt.spawn(async move {
            this.start_scan().await;
        });
    }

    pub fn trigger_probe_custom(
        self: &Arc<Self>,
        rt: &tokio::runtime::Handle,
        ip: IpAddr,
        custom_name: Option<String>,
        protocol_override: Option<CastProtocol>,
        kind: DeviceKind,
    ) {
        let this = self.clone();
        rt.spawn(async move {
            let _ = this
                .probe_and_add_custom(ip, custom_name, protocol_override, kind)
                .await;
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_samsung_lg_and_filter_router_xml() {
        let lg_xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName>[LG] webOS TV OLED55</friendlyName>
    <manufacturer>LG Electronics</manufacturer>
    <modelName>OLED55C1</modelName>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>/AVTransport/control</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:ConnectionManager:1</serviceType>
        <controlURL>/ConnectionManager/control</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

        let parsed = parse_upnp_device_xml("http://192.168.1.50:1990/desc.xml", lg_xml)
            .expect("LG MediaRenderer should parse");
        assert_eq!(parsed.friendly_name, "[LG] webOS TV OLED55");
        assert_eq!(parsed.manufacturer.as_deref(), Some("LG Electronics"));
        assert_eq!(
            parsed.av_transport_control_url.as_deref(),
            Some("http://192.168.1.50:1990/AVTransport/control")
        );
        assert!(parsed.protocols.contains(&CastProtocol::DlnaAvTransport));

        let router_xml = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:InternetGatewayDevice:1</deviceType>
    <friendlyName>Home WiFi Router</friendlyName>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:WANIPConnection:1</serviceType>
        <controlURL>/ctl/IPConn</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;
        assert!(
            parse_upnp_device_xml("http://192.168.1.1:5000/rootDesc.xml", router_xml).is_none(),
            "Router InternetGatewayDevice must be filtered out"
        );
    }

    fn encode_dns_name(buf: &mut Vec<u8>, name: &str) {
        for label in name.split('.') {
            buf.push(label.len() as u8);
            buf.extend_from_slice(label.as_bytes());
        }
        buf.push(0);
    }

    #[test]
    fn parse_mdns_googlecast_packet() {
        let mut pkt = vec![
            0x00, 0x00, 0x84, 0x00, // response header
            0x00, 0x00, // qd = 0
            0x00, 0x01, // an = 1 (PTR)
            0x00, 0x00, // ns = 0
            0x00, 0x03, // ar = 3 (SRV, TXT, A)
        ];

        let svc = "_googlecast._tcp.local";
        let inst = "RuangTamu._googlecast._tcp.local";
        let host = "chromecast-1.local";

        // PTR
        encode_dns_name(&mut pkt, svc);
        pkt.extend_from_slice(&12u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&120u32.to_be_bytes());
        let mut ptr_data = Vec::new();
        encode_dns_name(&mut ptr_data, inst);
        pkt.extend_from_slice(&(ptr_data.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&ptr_data);

        // SRV
        encode_dns_name(&mut pkt, inst);
        pkt.extend_from_slice(&33u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&120u32.to_be_bytes());
        let mut srv_data = Vec::new();
        srv_data.extend_from_slice(&0u16.to_be_bytes());
        srv_data.extend_from_slice(&0u16.to_be_bytes());
        srv_data.extend_from_slice(&8009u16.to_be_bytes());
        encode_dns_name(&mut srv_data, host);
        pkt.extend_from_slice(&(srv_data.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&srv_data);

        // TXT
        encode_dns_name(&mut pkt, inst);
        pkt.extend_from_slice(&16u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&120u32.to_be_bytes());
        let mut txt_data = Vec::new();
        for kv in [b"fn=Ruang Tamu TV".as_slice(), b"md=BRAVIA 4K"] {
            txt_data.push(kv.len() as u8);
            txt_data.extend_from_slice(kv);
        }
        pkt.extend_from_slice(&(txt_data.len() as u16).to_be_bytes());
        pkt.extend_from_slice(&txt_data);

        // A
        encode_dns_name(&mut pkt, host);
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&1u16.to_be_bytes());
        pkt.extend_from_slice(&120u32.to_be_bytes());
        pkt.extend_from_slice(&4u16.to_be_bytes());
        pkt.extend_from_slice(&[192, 168, 1, 77]);

        let parsed = parse_mdns_response(&pkt);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].protocol, CastProtocol::GoogleCast);
        assert_eq!(parsed[0].port, 8009);
        assert_eq!(parsed[0].ip, Some(Ipv4Addr::new(192, 168, 1, 77)));
        assert_eq!(parsed[0].friendly_name.as_deref(), Some("Ruang Tamu TV"));
        assert_eq!(parsed[0].model.as_deref(), Some("BRAVIA 4K"));
    }
}
