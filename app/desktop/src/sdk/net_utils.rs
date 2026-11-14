use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::time::Duration;

use if_addrs::IfAddr;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NetworkKind {
    Wifi,
    Lan,
}

impl std::fmt::Display for NetworkKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NetworkKind::Wifi => write!(f, "Wi-Fi"),
            NetworkKind::Lan => write!(f, "LAN"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct LocalInterface {
    #[allow(dead_code)]
    pub name: String,
    pub ip: Ipv4Addr,
    pub netmask: Ipv4Addr,
    pub kind: NetworkKind,
}

impl LocalInterface {
    pub fn same_subnet(&self, target: Ipv4Addr) -> bool {
        let mask = u32::from(self.netmask);
        if mask == 0 {
            return self.ip.octets()[..3] == target.octets()[..3];
        }
        (u32::from(self.ip) & mask) == (u32::from(target) & mask)
    }

    /// Enumerates neighbor IPv4 host addresses in this interface's subnet,
    /// supporting `/23`/`/22` networks while bounding very wide masks (`/16`)
    /// to the local `/24`.
    pub fn subnet_ipv4_hosts(&self, max_hosts: usize) -> Vec<Ipv4Addr> {
        let ip_u32 = u32::from(self.ip);
        let mask_u32 = u32::from(self.netmask);
        let valid_mask = mask_u32 != 0 && (!mask_u32).wrapping_add(1).is_power_of_two();

        let (start, end) = if valid_mask {
            let net = ip_u32 & mask_u32;
            let bcast = net | !mask_u32;
            let span = bcast.saturating_sub(net) as usize;
            if span >= 2 && span <= max_hosts + 2 {
                (net.saturating_add(1), bcast)
            } else {
                let net24 = ip_u32 & 0xFFFF_FF00;
                (net24 + 1, net24 + 255)
            }
        } else {
            let net24 = ip_u32 & 0xFFFF_FF00;
            (net24 + 1, net24 + 255)
        };

        let mut hosts = Vec::with_capacity((end - start) as usize);
        for addr in start..end {
            if addr == ip_u32 {
                continue;
            }
            hosts.push(Ipv4Addr::from(addr));
        }
        hosts
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WifiDirectBeacon {
    pub ssid: String,
    pub signal: Option<u8>,
    pub channel: Option<u16>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WifiNetworkContext {
    pub active_ssid: Option<String>,
    pub wifi_direct_tvs: Vec<WifiDirectBeacon>,
}

fn split_nmcli_terse_line(line: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                cur.push(next);
                chars.next();
            } else {
                cur.push(c);
            }
        } else if c == ':' {
            fields.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    fields.push(cur);
    fields
}

/// Parses `nmcli -t -f ACTIVE,SSID,SIGNAL,CHAN dev wifi list` lines into active
/// SSID and nearby Wi-Fi Direct (`DIRECT-*`) beacons.
pub fn parse_nmcli_wifi_list(output: &str) -> WifiNetworkContext {
    let mut ctx = WifiNetworkContext::default();
    let mut seen_direct = std::collections::HashSet::new();

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let cols = split_nmcli_terse_line(trimmed);
        if cols.len() < 2 {
            continue;
        }
        let active = cols[0].eq_ignore_ascii_case("yes") || cols[0] == "*";
        let ssid = cols[1].trim();
        if ssid.is_empty() {
            continue;
        }
        let signal = cols.get(2).and_then(|s| s.trim().parse::<u8>().ok());
        let channel = cols.get(3).and_then(|s| s.trim().parse::<u16>().ok());

        if active && ctx.active_ssid.is_none() {
            ctx.active_ssid = Some(ssid.to_string());
        }
        if ssid.to_ascii_uppercase().starts_with("DIRECT-") && seen_direct.insert(ssid.to_string())
        {
            ctx.wifi_direct_tvs.push(WifiDirectBeacon {
                ssid: ssid.to_string(),
                signal,
                channel,
            });
        }
    }
    ctx
}

/// Reads the OS Wi-Fi context non-destructively from NetworkManager cache.
pub async fn read_wifi_network_context() -> Option<WifiNetworkContext> {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(|| {
            let out = std::process::Command::new("nmcli")
                .args([
                    "-t",
                    "-f",
                    "ACTIVE,SSID,SIGNAL,CHAN",
                    "dev",
                    "wifi",
                    "list",
                    "--rescan",
                    "no",
                ])
                .output()
                .ok()?;
            if !out.status.success() {
                return None;
            }
            let text = String::from_utf8_lossy(&out.stdout);
            Some(parse_nmcli_wifi_list(&text))
        })
        .await
        .ok()
        .flatten()
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Classifies an OS interface name into physical `Wifi` or `Lan`, returning
/// `None` for loopback, container bridges, VPN tunnels, and virtual interfaces.
pub fn classify_interface_kind(name: &str) -> Option<NetworkKind> {
    let lower = name.to_ascii_lowercase();
    let ignored_prefixes = [
        "lo",
        "docker",
        "br-",
        "veth",
        "virbr",
        "tun",
        "tap",
        "wg",
        "tailscale",
        "zt",
        "vmnet",
        "vboxnet",
        "podman",
        "cni",
        "flannel",
        "cali",
        "dummy",
        "ifb",
    ];
    if ignored_prefixes
        .iter()
        .any(|p| lower == *p || lower.starts_with(p))
    {
        return None;
    }

    if lower.starts_with("wl")
        || lower.starts_with("wifi")
        || lower.contains("wi-fi")
        || lower.contains("wireless")
    {
        return Some(NetworkKind::Wifi);
    }

    Some(NetworkKind::Lan)
}

/// Enumerates active non-virtual IPv4 interfaces on this machine.
pub fn list_active_ipv4_interfaces() -> Vec<LocalInterface> {
    let Ok(addrs) = if_addrs::get_if_addrs() else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for iface in addrs {
        if iface.is_loopback() {
            continue;
        }
        let Some(kind) = classify_interface_kind(&iface.name) else {
            continue;
        };
        if let IfAddr::V4(v4) = iface.addr {
            let ip = v4.ip;
            if ip.is_loopback() || ip.is_link_local() || ip.is_unspecified() || ip.is_multicast() {
                continue;
            }
            out.push(LocalInterface {
                name: iface.name,
                ip,
                netmask: v4.netmask,
                kind,
            });
        }
    }
    out
}

/// Resolves the best non-loopback local IPv4 address used by the OS to reach
/// `target_ip`.
pub fn resolve_lan_ip_for_target(target_ip: Option<IpAddr>) -> Option<Ipv4Addr> {
    if let Some(IpAddr::V4(target_v4)) = target_ip
        && !target_v4.is_unspecified()
        && let Ok(sock) = UdpSocket::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, 0)))
        && sock.connect(SocketAddr::from((target_v4, 80))).is_ok()
        && let Ok(SocketAddr::V4(local)) = sock.local_addr()
    {
        let ip = *local.ip();
        if !ip.is_loopback() && !ip.is_unspecified() {
            return Some(ip);
        }
    }

    let ifaces = list_active_ipv4_interfaces();
    if let Some(IpAddr::V4(target_v4)) = target_ip
        && let Some(found) = ifaces.iter().find(|i| i.same_subnet(target_v4))
    {
        return Some(found.ip);
    }

    ifaces
        .iter()
        .find(|i| i.kind == NetworkKind::Wifi)
        .or_else(|| ifaces.first())
        .map(|i| i.ip)
}

/// Infers whether a peer IP address is reachable over local `Wifi` or `Lan`
/// interface.
pub fn infer_network_kind_for_ip(ip: IpAddr, ifaces: &[LocalInterface]) -> NetworkKind {
    if let IpAddr::V4(target_v4) = ip {
        if let Some(local_ip) = resolve_lan_ip_for_target(Some(ip))
            && let Some(found) = ifaces.iter().find(|i| i.ip == local_ip)
        {
            return found.kind;
        }
        if let Some(found) = ifaces.iter().find(|i| i.same_subnet(target_v4)) {
            return found.kind;
        }
    }

    ifaces
        .iter()
        .find(|i| i.kind == NetworkKind::Wifi)
        .map(|_| NetworkKind::Wifi)
        .unwrap_or(NetworkKind::Lan)
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    #[allow(dead_code)]
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    #[allow(dead_code)]
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Decodes HTTP `Transfer-Encoding: chunked` payloads.
pub fn decode_chunked_body(raw: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut pos = 0usize;
    while pos < raw.len() {
        let line_end = raw[pos..].windows(2).position(|w| w == b"\r\n")? + pos;
        let size_hex = std::str::from_utf8(&raw[pos..line_end])
            .ok()?
            .split(';')
            .next()?
            .trim();
        let chunk_len = usize::from_str_radix(size_hex, 16).ok()?;
        pos = line_end + 2;
        if chunk_len == 0 {
            return Some(out);
        }
        if pos + chunk_len > raw.len() {
            out.extend_from_slice(&raw[pos..]);
            return Some(out);
        }
        out.extend_from_slice(&raw[pos..pos + chunk_len]);
        pos = (pos + chunk_len + 2).min(raw.len());
    }
    Some(out)
}

/// Sends a lightweight HTTP/1.1 request over plain TCP (intended for local LAN
/// TV control APIs).
pub async fn http_request(
    method: &str,
    raw_url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
    timeout: Duration,
) -> anyhow::Result<HttpResponse> {
    let parsed = Url::parse(raw_url)?;
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow::anyhow!("missing host in url {}", raw_url))?;
    let port = parsed.port_or_known_default().unwrap_or(80);
    let mut path_and_query = parsed.path().to_string();
    if path_and_query.is_empty() {
        path_and_query.push('/');
    }
    if let Some(q) = parsed.query() {
        path_and_query.push('?');
        path_and_query.push_str(q);
    }

    let addr = format!("{}:{}", host, port);
    let mut stream = tokio::time::timeout(timeout, TcpStream::connect(&addr)).await??;
    let _ = stream.set_nodelay(true);

    let host_hdr = if (port == 80 && parsed.scheme() == "http") || host.contains(':') {
        host.to_string()
    } else {
        format!("{}:{}", host, port)
    };

    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        method, path_and_query, host_hdr
    );
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    if let Some(b) = body {
        req.push_str(&format!("Content-Length: {}\r\n", b.len()));
    }
    req.push_str("\r\n");

    tokio::time::timeout(timeout, async {
        stream.write_all(req.as_bytes()).await?;
        if let Some(b) = body {
            stream.write_all(b).await?;
        }
        stream.flush().await?;
        Ok::<(), std::io::Error>(())
    })
    .await??;

    let mut raw = Vec::with_capacity(4096);
    let mut tmp = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if let Some(hdr_end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let hdr_slice = String::from_utf8_lossy(&raw[..hdr_end]);
            let mut content_len: Option<usize> = None;
            let mut is_chunked = false;
            for line in hdr_slice.lines().skip(1) {
                if let Some((k, v)) = line.split_once(':') {
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        content_len = v.trim().parse::<usize>().ok();
                    } else if k.trim().eq_ignore_ascii_case("transfer-encoding")
                        && v.to_ascii_lowercase().contains("chunked")
                    {
                        is_chunked = true;
                    }
                }
            }
            let body_so_far = &raw[hdr_end + 4..];
            if let Some(cl) = content_len
                && body_so_far.len() >= cl
            {
                break;
            }
            if is_chunked
                && (body_so_far.starts_with(b"0\r\n\r\n")
                    || body_so_far.windows(5).any(|w| w == b"\r\n0\r\n")
                        && body_so_far.ends_with(b"\r\n"))
            {
                break;
            }
        }

        let remain = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remain.is_zero() {
            break;
        }
        match tokio::time::timeout(remain, stream.read(&mut tmp)).await {
            Ok(Ok(0)) => break,
            Ok(Ok(n)) => {
                raw.extend_from_slice(&tmp[..n]);
                if raw.len() > 2 * 1024 * 1024 {
                    break;
                }
            }
            Ok(Err(_)) | Err(_) => break,
        }
    }

    let hdr_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| anyhow::anyhow!("malformed HTTP response from {}", raw_url))?;
    let hdr_text = String::from_utf8_lossy(&raw[..hdr_end]);
    let mut lines = hdr_text.lines();
    let status_line = lines.next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let mut resp_headers = Vec::new();
    let mut is_chunked = false;
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            let key = k.trim().to_string();
            let val = v.trim().to_string();
            if key.eq_ignore_ascii_case("transfer-encoding")
                && val.to_ascii_lowercase().contains("chunked")
            {
                is_chunked = true;
            }
            resp_headers.push((key, val));
        }
    }

    let body_raw = &raw[hdr_end + 4..];
    let body = if is_chunked {
        decode_chunked_body(body_raw).unwrap_or_else(|| body_raw.to_vec())
    } else {
        body_raw.to_vec()
    };

    Ok(HttpResponse {
        status,
        headers: resp_headers,
        body,
    })
}

/// Checks whether a TCP port accepts connections within `timeout`.
pub async fn tcp_port_open(ip: IpAddr, port: u16, timeout: Duration) -> bool {
    tokio::time::timeout(timeout, TcpStream::connect(SocketAddr::new(ip, port)))
        .await
        .map(|res| res.is_ok())
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_filters_virtual_interfaces() {
        assert_eq!(classify_interface_kind("wlo1"), Some(NetworkKind::Wifi));
        assert_eq!(classify_interface_kind("wlan0"), Some(NetworkKind::Wifi));
        assert_eq!(classify_interface_kind("enp3s0"), Some(NetworkKind::Lan));
        assert_eq!(classify_interface_kind("eth0"), Some(NetworkKind::Lan));
        assert_eq!(classify_interface_kind("docker0"), None);
        assert_eq!(classify_interface_kind("br-a1b2c3"), None);
        assert_eq!(classify_interface_kind("veth1234"), None);
        assert_eq!(classify_interface_kind("lo"), None);
        assert_eq!(classify_interface_kind("tun0"), None);
    }

    #[test]
    fn subnet_hosts_handles_slash_23_and_clamps_wide_masks() {
        let slash23 = LocalInterface {
            name: "wlo1".into(),
            ip: Ipv4Addr::new(192, 168, 80, 179),
            netmask: Ipv4Addr::new(255, 255, 254, 0),
            kind: NetworkKind::Wifi,
        };
        let hosts = slash23.subnet_ipv4_hosts(1024);
        assert_eq!(hosts.len(), 509);
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 80, 94)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 81, 1)));
        assert!(hosts.contains(&Ipv4Addr::new(192, 168, 81, 254)));
        assert!(!hosts.contains(&Ipv4Addr::new(192, 168, 80, 179)));

        let slash16 = LocalInterface {
            name: "eth0".into(),
            ip: Ipv4Addr::new(10, 20, 30, 40),
            netmask: Ipv4Addr::new(255, 255, 0, 0),
            kind: NetworkKind::Lan,
        };
        let clamped = slash16.subnet_ipv4_hosts(1024);
        assert_eq!(clamped.len(), 253);
        assert!(clamped.contains(&Ipv4Addr::new(10, 20, 30, 1)));
        assert!(clamped.contains(&Ipv4Addr::new(10, 20, 30, 254)));
    }

    #[test]
    fn parses_nmcli_wifi_list_with_direct_tv() {
        let sample = "\
yes:Lab-IoT:80:11
no:DIRECT-vM-POLYTRON4K:92:11
no:Zeee\\:Home:90:1
";
        let ctx = parse_nmcli_wifi_list(sample);
        assert_eq!(ctx.active_ssid.as_deref(), Some("Lab-IoT"));
        assert_eq!(ctx.wifi_direct_tvs.len(), 1);
        assert_eq!(ctx.wifi_direct_tvs[0].ssid, "DIRECT-vM-POLYTRON4K");
        assert_eq!(ctx.wifi_direct_tvs[0].signal, Some(92));
        assert_eq!(ctx.wifi_direct_tvs[0].channel, Some(11));
    }
}
