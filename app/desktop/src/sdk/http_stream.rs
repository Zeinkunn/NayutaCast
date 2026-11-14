use std::{
    collections::{HashMap, VecDeque},
    net::{Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{RwLock, broadcast},
};

use codec::UniversalLiveMuxer;

const DLNA_FEATURES_TS: &str = "DLNA.ORG_PN=AVC_TS_MP_HD_AAC_MULT5;DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000";

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Shared live media hub fed by `UniversalLiveMuxer` and served over HTTP to
/// Smart TVs.
pub struct StreamHub {
    open_stream_clients: AtomicUsize,
    last_hls_activity_ms: AtomicU64,
    bytes_served: AtomicU64,
    mp4_frag_seq: AtomicU64,
    stopped: AtomicBool,
    mpegts_gop_prefix: RwLock<Vec<u8>>,
    fmp4_init: RwLock<Vec<u8>>,
    fmp4_latest_frag: RwLock<Option<(u64, Vec<u8>)>>,
    hls_playlist: RwLock<String>,
    hls_segments: RwLock<HashMap<String, Vec<u8>>>,
    hls_order: RwLock<VecDeque<String>>,
    ts_tx: broadcast::Sender<Vec<u8>>,
    mp4_tx: broadcast::Sender<(u64, Vec<u8>)>,
}

impl StreamHub {
    pub fn new() -> Arc<Self> {
        let (ts_tx, _) = broadcast::channel(256);
        let (mp4_tx, _) = broadcast::channel(64);
        Arc::new(Self {
            open_stream_clients: AtomicUsize::new(0),
            last_hls_activity_ms: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
            mp4_frag_seq: AtomicU64::new(0),
            stopped: AtomicBool::new(false),
            mpegts_gop_prefix: RwLock::new(Vec::new()),
            fmp4_init: RwLock::new(Vec::new()),
            fmp4_latest_frag: RwLock::new(None),
            hls_playlist: RwLock::new(
                "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:2\n".to_string(),
            ),
            hls_segments: RwLock::new(HashMap::new()),
            hls_order: RwLock::new(VecDeque::new()),
            ts_tx,
            mp4_tx,
        })
    }

    pub fn active_clients(&self) -> usize {
        let long_lived = self.open_stream_clients.load(Ordering::SeqCst);
        let last_hls = self.last_hls_activity_ms.load(Ordering::SeqCst);
        let hls_alive = usize::from(last_hls > 0 && now_ms().saturating_sub(last_hls) <= 6_000);
        long_lived + hls_alive
    }

    pub fn bytes_served(&self) -> u64 {
        self.bytes_served.load(Ordering::SeqCst)
    }

    pub fn shutdown(&self) {
        self.stopped.store(true, Ordering::SeqCst);
    }

    /// Synchronizes freshly encoded TS / fMP4 / HLS outputs from
    /// `UniversalLiveMuxer`.
    pub async fn sync_from_muxer(&self, muxer: &UniversalLiveMuxer, out: codec::MuxerOutput) {
        if self.fmp4_init.read().await.is_empty() && !muxer.fmp4_init_segment().is_empty() {
            *self.fmp4_init.write().await = muxer.fmp4_init_segment().to_vec();
        }
        {
            *self.mpegts_gop_prefix.write().await = muxer.mpegts_gop_prefix().to_vec();
        }
        if let Some(frag) = out.mp4_fragment {
            let seq = self.mp4_frag_seq.fetch_add(1, Ordering::SeqCst) + 1;
            *self.fmp4_latest_frag.write().await = Some((seq, frag.clone()));
            let _ = self.mp4_tx.send((seq, frag));
        }
        for chunk in out.mpegts_chunks {
            let _ = self.ts_tx.send(chunk);
        }

        let playlist = muxer.hls_playlist();
        *self.hls_playlist.write().await = playlist.to_string();

        for line in playlist.lines() {
            if line.starts_with("seg_") && line.ends_with(".ts") {
                let already = self.hls_segments.read().await.contains_key(line);
                if !already && let Some(bytes) = muxer.hls_segment(line) {
                    self.hls_segments
                        .write()
                        .await
                        .insert(line.to_string(), bytes.to_vec());
                    let mut order = self.hls_order.write().await;
                    order.push_back(line.to_string());
                    while order.len() > 8 {
                        if let Some(old) = order.pop_front() {
                            self.hls_segments.write().await.remove(&old);
                        }
                    }
                }
            }
        }
    }

    /// Directly seeds test/synthetic media bytes (used by unit/integration
    /// tests).
    #[allow(dead_code)]
    pub async fn publish_raw_test_stream(
        &self,
        ts_data: Vec<u8>,
        mp4_init: Vec<u8>,
        mp4_frag: Vec<u8>,
    ) {
        *self.mpegts_gop_prefix.write().await = ts_data.clone();
        *self.fmp4_init.write().await = mp4_init;
        let seq = self.mp4_frag_seq.fetch_add(1, Ordering::SeqCst) + 1;
        *self.fmp4_latest_frag.write().await = Some((seq, mp4_frag.clone()));
        let _ = self.ts_tx.send(ts_data);
        let _ = self.mp4_tx.send((seq, mp4_frag));
    }
}

struct ClientGuard {
    hub: Arc<StreamHub>,
}

impl ClientGuard {
    fn new(hub: Arc<StreamHub>) -> Self {
        hub.open_stream_clients.fetch_add(1, Ordering::SeqCst);
        Self { hub }
    }
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.hub.open_stream_clients.fetch_sub(1, Ordering::SeqCst);
    }
}

pub struct HttpStreamServer {
    pub port: u16,
    pub hub: Arc<StreamHub>,
    handle: Option<tokio::task::JoinHandle<()>>,
}

async fn bind_preferred_http_listener(bind_ip: Ipv4Addr) -> anyhow::Result<TcpListener> {
    if let Ok(raw) = std::env::var("NAYUTACAST_HTTP_PORT")
        && let Ok(port) = raw.trim().parse::<u16>()
        && port > 0
        && let Ok(listener) = TcpListener::bind(SocketAddr::from((bind_ip, port))).await
    {
        return Ok(listener);
    }

    if !bind_ip.is_loopback() {
        for port in 19080..=19085 {
            if let Ok(listener) = TcpListener::bind(SocketAddr::from((bind_ip, port))).await {
                return Ok(listener);
            }
        }
    }

    Ok(TcpListener::bind(SocketAddr::from((bind_ip, 0))).await?)
}

impl HttpStreamServer {
    pub async fn start(bind_ip: Ipv4Addr, hub: Arc<StreamHub>) -> anyhow::Result<Self> {
        let listener = bind_preferred_http_listener(bind_ip).await?;
        let port = listener.local_addr()?.port();
        log::info!(
            "HttpStreamServer listening on http://{}:{} (watch: /watch)",
            bind_ip,
            port
        );
        let hub_srv = hub.clone();

        let handle = tokio::spawn(async move {
            loop {
                if hub_srv.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok((stream, peer)) = listener.accept().await else {
                    break;
                };
                let hub_conn = hub_srv.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, peer, hub_conn).await {
                        log::debug!("HTTP client {} disconnected: {}", peer, e);
                    }
                });
            }
        });

        Ok(Self {
            port,
            hub,
            handle: Some(handle),
        })
    }

    pub fn stop(&mut self) {
        self.hub.shutdown();
        if let Some(h) = self.handle.take() {
            h.abort();
        }
    }
}

impl Drop for HttpStreamServer {
    fn drop(&mut self) {
        self.stop();
    }
}

async fn handle_client(
    stream: TcpStream,
    peer: SocketAddr,
    hub: Arc<StreamHub>,
) -> anyhow::Result<()> {
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    let n =
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut request_line)).await??;
    if n == 0 {
        return Ok(());
    }

    // Parse relevant headers and drain until blank line.
    let mut range_hdr: Option<String> = None;
    loop {
        let mut line = String::new();
        let len =
            tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await??;
        if len == 0 || line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((k, v)) = line.split_once(':')
            && k.trim().eq_ignore_ascii_case("range")
        {
            range_hdr = Some(v.trim().to_string());
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let raw_target = parts.next().unwrap_or("/");
    let path = raw_target.split('?').next().unwrap_or("/");
    log::info!(
        "HTTP stream request from {}: {} {}{}",
        peer.ip(),
        method,
        path,
        range_hdr
            .as_deref()
            .map(|r| format!(" (Range: {})", r))
            .unwrap_or_default()
    );

    if method.eq_ignore_ascii_case("OPTIONS") {
        write_half
            .write_all(
                b"HTTP/1.1 204 No Content\r\n\
                  Access-Control-Allow-Origin: *\r\n\
                  Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n\
                  Access-Control-Allow-Headers: *\r\n\
                  Connection: close\r\n\r\n",
            )
            .await?;
        return Ok(());
    }

    let is_head = method.eq_ignore_ascii_case("HEAD");

    match path {
        "/live.ts" | "/live.mpeg" => {
            let content_type = if path == "/live.mpeg" {
                "video/mpeg"
            } else {
                "video/mp2t"
            };
            let hdr = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: {}\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Cache-Control: no-cache, no-store\r\n\
                 Pragma: no-cache\r\n\
                 Accept-Ranges: none\r\n\
                 transferMode.dlna.org: Streaming\r\n\
                 contentFeatures.dlna.org: {}\r\n\
                 Connection: close\r\n\r\n",
                content_type, DLNA_FEATURES_TS
            );
            write_half.write_all(hdr.as_bytes()).await?;
            write_half.flush().await?;
            if is_head {
                return Ok(());
            }

            let _guard = ClientGuard::new(hub.clone());
            let mut rx = hub.ts_tx.subscribe();

            // Wait briefly for initial keyframe GOP prefix if stream just
            // started.
            for _ in 0..25 {
                if !hub.mpegts_gop_prefix.read().await.is_empty()
                    || hub.stopped.load(Ordering::SeqCst)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
            }

            let prefix = hub.mpegts_gop_prefix.read().await.clone();
            if !prefix.is_empty() {
                write_half.write_all(&prefix).await?;
                hub.bytes_served
                    .fetch_add(prefix.len() as u64, Ordering::Relaxed);
            }

            while !hub.stopped.load(Ordering::SeqCst) {
                match rx.recv().await {
                    Ok(chunk) => {
                        if write_half.write_all(&chunk).await.is_err() {
                            break;
                        }
                        hub.bytes_served
                            .fetch_add(chunk.len() as u64, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        "/live.mp4" => {
            let hdr = "HTTP/1.1 200 OK\r\n\
                       Content-Type: video/mp4\r\n\
                       Access-Control-Allow-Origin: *\r\n\
                       Cache-Control: no-cache, no-store\r\n\
                       Accept-Ranges: none\r\n\
                       Connection: close\r\n\r\n";
            write_half.write_all(hdr.as_bytes()).await?;
            write_half.flush().await?;
            if is_head {
                return Ok(());
            }

            let _guard = ClientGuard::new(hub.clone());

            // Wait until both ftyp+moov init AND at least one keyframe-aligned
            // moof fragment are ready before flushing response headers/body, so
            // receivers never stall in LOADING after receiving only header
            // bytes.
            for _ in 0..70 {
                if hub.stopped.load(Ordering::SeqCst)
                    || (!hub.fmp4_init.read().await.is_empty()
                        && hub.fmp4_latest_frag.read().await.is_some())
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }

            let mut rx = hub.mp4_tx.subscribe();
            let init = hub.fmp4_init.read().await.clone();
            if !init.is_empty() {
                write_half.write_all(&init).await?;
                hub.bytes_served
                    .fetch_add(init.len() as u64, Ordering::Relaxed);
            }
            let mut sent_seq = 0u64;
            let mut init_frag_len = 0usize;
            if let Some((seq, frag)) = hub.fmp4_latest_frag.read().await.clone() {
                init_frag_len = frag.len();
                write_half.write_all(&frag).await?;
                hub.bytes_served
                    .fetch_add(frag.len() as u64, Ordering::Relaxed);
                sent_seq = seq;
            }
            let _ = write_half.flush().await;
            log::info!(
                "HTTP /live.mp4 streaming started to {} (init={}B, initial_frag_seq={} ({}B))",
                peer.ip(),
                init.len(),
                sent_seq,
                init_frag_len
            );

            while !hub.stopped.load(Ordering::SeqCst) {
                match rx.recv().await {
                    Ok((seq, frag)) => {
                        if seq <= sent_seq {
                            continue;
                        }
                        sent_seq = seq;
                        if write_half.write_all(&frag).await.is_err() {
                            break;
                        }
                        hub.bytes_served
                            .fetch_add(frag.len() as u64, Ordering::Relaxed);
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
        "/live.m3u8" => {
            hub.last_hls_activity_ms.store(now_ms(), Ordering::SeqCst);
            for _ in 0..30 {
                if !hub.hls_segments.read().await.is_empty() || hub.stopped.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let body = hub.hls_playlist.read().await.clone();
            let hdr = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: application/vnd.apple.mpegurl\r\n\
                 Content-Length: {}\r\n\
                 Access-Control-Allow-Origin: *\r\n\
                 Cache-Control: no-cache, no-store\r\n\
                 Connection: close\r\n\r\n",
                body.len()
            );
            write_half.write_all(hdr.as_bytes()).await?;
            if !is_head {
                write_half.write_all(body.as_bytes()).await?;
                hub.bytes_served
                    .fetch_add(body.len() as u64, Ordering::Relaxed);
            }
        }
        p if p.starts_with("/seg_") && p.ends_with(".ts") => {
            hub.last_hls_activity_ms.store(now_ms(), Ordering::SeqCst);
            let seg_name = p.trim_start_matches('/');
            if let Some(data) = hub.hls_segments.read().await.get(seg_name).cloned() {
                let total = data.len();
                let parsed_range = range_hdr.as_deref().and_then(|r| {
                    let spec = r.strip_prefix("bytes=")?;
                    let (s_str, e_str) = spec.split_once('-')?;
                    let start = s_str.trim().parse::<usize>().ok()?;
                    let end = if e_str.trim().is_empty() {
                        total.checked_sub(1)?
                    } else {
                        e_str
                            .trim()
                            .parse::<usize>()
                            .ok()?
                            .min(total.checked_sub(1)?)
                    };
                    (start <= end && start < total).then_some((start, end))
                });

                if let Some((start, end)) = parsed_range {
                    let slice = &data[start..=end];
                    let hdr = format!(
                        "HTTP/1.1 206 Partial Content\r\n\
                         Content-Type: video/mp2t\r\n\
                         Accept-Ranges: bytes\r\n\
                         Content-Range: bytes {}-{}/{}\r\n\
                         Content-Length: {}\r\n\
                         Access-Control-Allow-Origin: *\r\n\
                         Cache-Control: no-cache\r\n\
                         Connection: close\r\n\r\n",
                        start,
                        end,
                        total,
                        slice.len()
                    );
                    write_half.write_all(hdr.as_bytes()).await?;
                    if !is_head {
                        write_half.write_all(slice).await?;
                        hub.bytes_served
                            .fetch_add(slice.len() as u64, Ordering::Relaxed);
                    }
                } else {
                    let hdr = format!(
                        "HTTP/1.1 200 OK\r\n\
                         Content-Type: video/mp2t\r\n\
                         Accept-Ranges: bytes\r\n\
                         Content-Length: {}\r\n\
                         Access-Control-Allow-Origin: *\r\n\
                         Cache-Control: no-cache\r\n\
                         Connection: close\r\n\r\n",
                        total
                    );
                    write_half.write_all(hdr.as_bytes()).await?;
                    if !is_head {
                        write_half.write_all(&data).await?;
                        hub.bytes_served.fetch_add(total as u64, Ordering::Relaxed);
                    }
                }
            } else {
                write_half
                    .write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    )
                    .await?;
            }
        }
        "/" | "/watch" => {
            let html = WATCH_HTML;
            let hdr = format!(
                "HTTP/1.1 200 OK\r\n\
                 Content-Type: text/html; charset=utf-8\r\n\
                 Content-Length: {}\r\n\
                 Cache-Control: no-cache\r\n\
                 Connection: close\r\n\r\n",
                html.len()
            );
            write_half.write_all(hdr.as_bytes()).await?;
            if !is_head {
                write_half.write_all(html.as_bytes()).await?;
            }
        }
        _ => {
            write_half
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await?;
        }
    }

    let _ = write_half.flush().await;
    Ok(())
}

const WATCH_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>NayutaCast Live Screen</title>
<style>
  html,body { margin:0; padding:0; width:100%; height:100%; background:#06080c; color:#e2e8f0;
    font-family:-apple-system,BlinkMacSystemFont,Segoe UI,sans-serif; overflow:hidden; }
  .wrap { position:relative; width:100vw; height:100vh; display:flex; align-items:center; justify-content:center; }
  video { width:100%; height:100%; object-fit:contain; background:#000; }
  .badge { position:fixed; top:14px; left:14px; background:rgba(15,23,42,.72); border:1px solid rgba(148,163,184,.25);
    padding:6px 12px; border-radius:999px; font-size:13px; pointer-events:none; transition:opacity .4s; }
</style>
</head>
<body>
<div class="wrap">
  <video id="v" autoplay playsinline controls muted></video>
  <div id="st" class="badge">NayutaCast Live · Connecting...</div>
</div>
<script>
const v = document.getElementById('v');
const st = document.getElementById('st');
function startPlay() {
  const canHls = v.canPlayType('application/vnd.apple.mpegurl');
  v.src = canHls ? '/live.m3u8' : '/live.mp4';
  v.play().then(() => { st.textContent = 'NayutaCast Live'; setTimeout(() => st.style.opacity = '0.25', 3000); })
    .catch(() => { st.textContent = 'Tap screen to start playback'; });
}
v.addEventListener('error', () => {
  st.style.opacity = '1';
  st.textContent = 'Reconnecting stream...';
  setTimeout(startPlay, 1500);
});
startPlay();
</script>
</body>
</html>
"#;
