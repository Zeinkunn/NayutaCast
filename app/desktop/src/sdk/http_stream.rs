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
    stopped: AtomicBool,
    mpegts_gop_prefix: RwLock<Vec<u8>>,
    fmp4_init: RwLock<Vec<u8>>,
    fmp4_latest_frag: RwLock<Option<Vec<u8>>>,
    hls_playlist: RwLock<String>,
    hls_segments: RwLock<HashMap<String, Vec<u8>>>,
    hls_order: RwLock<VecDeque<String>>,
    ts_tx: broadcast::Sender<Vec<u8>>,
    mp4_tx: broadcast::Sender<Vec<u8>>,
}

impl StreamHub {
    pub fn new() -> Arc<Self> {
        let (ts_tx, _) = broadcast::channel(256);
        let (mp4_tx, _) = broadcast::channel(64);
        Arc::new(Self {
            open_stream_clients: AtomicUsize::new(0),
            last_hls_activity_ms: AtomicU64::new(0),
            bytes_served: AtomicU64::new(0),
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
            *self.fmp4_latest_frag.write().await = Some(frag.clone());
            let _ = self.mp4_tx.send(frag);
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
        *self.fmp4_latest_frag.write().await = Some(mp4_frag.clone());
        let _ = self.ts_tx.send(ts_data);
        let _ = self.mp4_tx.send(mp4_frag);
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

impl HttpStreamServer {
    pub async fn start(bind_ip: Ipv4Addr, hub: Arc<StreamHub>) -> anyhow::Result<Self> {
        let listener = TcpListener::bind(SocketAddr::from((bind_ip, 0))).await?;
        let port = listener.local_addr()?.port();
        let hub_srv = hub.clone();

        let handle = tokio::spawn(async move {
            loop {
                if hub_srv.stopped.load(Ordering::SeqCst) {
                    break;
                }
                let Ok((stream, _peer)) = listener.accept().await else {
                    break;
                };
                let hub_conn = hub_srv.clone();
                tokio::spawn(async move {
                    let _ = handle_client(stream, hub_conn).await;
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

async fn handle_client(stream: TcpStream, hub: Arc<StreamHub>) -> anyhow::Result<()> {
    let _ = stream.set_nodelay(true);
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    let mut request_line = String::new();
    let n =
        tokio::time::timeout(Duration::from_secs(3), reader.read_line(&mut request_line)).await??;
    if n == 0 {
        return Ok(());
    }

    // Drain remaining request headers until blank line.
    loop {
        let mut line = String::new();
        let len =
            tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut line)).await??;
        if len == 0 || line == "\r\n" || line == "\n" {
            break;
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("GET");
    let raw_target = parts.next().unwrap_or("/");
    let path = raw_target.split('?').next().unwrap_or("/");

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
            let mut rx = hub.mp4_tx.subscribe();

            for _ in 0..25 {
                if !hub.fmp4_init.read().await.is_empty() || hub.stopped.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(80)).await;
            }

            let init = hub.fmp4_init.read().await.clone();
            if !init.is_empty() {
                write_half.write_all(&init).await?;
                hub.bytes_served
                    .fetch_add(init.len() as u64, Ordering::Relaxed);
            }
            if let Some(frag) = hub.fmp4_latest_frag.read().await.clone() {
                write_half.write_all(&frag).await?;
                hub.bytes_served
                    .fetch_add(frag.len() as u64, Ordering::Relaxed);
            }

            while !hub.stopped.load(Ordering::SeqCst) {
                match rx.recv().await {
                    Ok(frag) => {
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
            for _ in 0..20 {
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
                let hdr = format!(
                    "HTTP/1.1 200 OK\r\n\
                     Content-Type: video/mp2t\r\n\
                     Content-Length: {}\r\n\
                     Access-Control-Allow-Origin: *\r\n\
                     Cache-Control: no-cache\r\n\
                     Connection: close\r\n\r\n",
                    data.len()
                );
                write_half.write_all(hdr.as_bytes()).await?;
                if !is_head {
                    write_half.write_all(&data).await?;
                    hub.bytes_served
                        .fetch_add(data.len() as u64, Ordering::Relaxed);
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
