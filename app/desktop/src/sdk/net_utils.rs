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
}

/// Classifies an OS interface name into physical `Wifi` or `Lan`, returning `None`
/// for loopback, container bridges, VPN tunnels, and virtual interfaces.
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

/// Resolves the best non-loopback local IPv4 address used by the OS to reach `target_ip`.
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

/// Infers whether a peer IP address is reachable over local `Wifi` or `Lan` interface.
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

/// Sends a lightweight HTTP/1.1 request over plain TCP (intended for local LAN TV control APIs).
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
}
