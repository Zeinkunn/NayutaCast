use std::{
    ffi::c_void,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

use capture::{
    AudioCaptureSourceDescription, Capture, CaptureOptions, DisplayMode, FrameConsumer,
    SourceCaptureOptions, VideoCaptureSourceDescription, VirtualDisplayGuard,
};
use codec::{LiveMuxerOptions, UniversalLiveMuxer};
use common::{
    Size, TransportOptions, TransportStrategy,
    codec::VideoEncoderType,
    frame::{AudioFrame, VideoFormat, VideoFrame, VideoSubFormat},
};
use egui::ColorImage;
use nayutacast::{
    AudioOptions, MediaStreamSink, NayutacastSenderMediaOptions, NayutacastSenderOptions,
    NayutacastSenderTrackOptions, Source, SourceType, VideoOptions,
};
use parking_lot::Mutex;
use tokio::sync::RwLock;

use crate::config::AppSettings;
use crate::sdk::{
    capture::LiveFrameSink,
    cast::{
        ActiveCastBackend, CastContext, CastingState, browser_watch_url,
        connect_smart_tv_protocols,
        nayutacast::{NayutacastCastSession, SrtSenderHandle},
        start_browser_watch,
    },
    discovery::{CastProtocol, DeviceInfo, DeviceKind, DeviceManager},
    http_stream::{HttpStreamServer, StreamHub},
    net_utils,
};

enum EncoderMsg {
    Video(OwnedVideoFrame),
    Audio(OwnedAudioFrame),
}

struct OwnedVideoFrame {
    width: u32,
    height: u32,
    format: VideoFormat,
    linesize: [usize; 2],
    plane0: Vec<u8>,
    plane1: Vec<u8>,
    plane2: Vec<u8>,
}

struct OwnedAudioFrame {
    sample_rate: u32,
    frames: u32,
    pcm_i16: Vec<i16>,
}

impl OwnedVideoFrame {
    fn from_borrowed(src: &VideoFrame) -> Option<Self> {
        if src.sub_format != VideoSubFormat::SW || src.data[0].is_null() {
            return None;
        }
        let w = src.width as usize;
        let h = src.height as usize;
        if w < 2 || h < 2 {
            return None;
        }

        match src.format {
            VideoFormat::NV12 => {
                if src.data[1].is_null() {
                    return None;
                }
                let ls0 = (src.linesize[0] as usize).max(w);
                let ls1 = (src.linesize[1] as usize).max(w);
                let p0 = unsafe { std::slice::from_raw_parts(src.data[0] as *const u8, ls0 * h) };
                let p1 =
                    unsafe { std::slice::from_raw_parts(src.data[1] as *const u8, ls1 * (h / 2)) };
                Some(Self {
                    width: src.width,
                    height: src.height,
                    format: VideoFormat::NV12,
                    linesize: [ls0, ls1],
                    plane0: p0.to_vec(),
                    plane1: p1.to_vec(),
                    plane2: Vec::new(),
                })
            }
            VideoFormat::I420 => {
                if src.data[1].is_null() || src.data[2].is_null() {
                    return None;
                }
                let ls0 = (src.linesize[0] as usize).max(w);
                let ls1 = (src.linesize[1] as usize).max(w / 2);
                let ls2 = (src.linesize[2] as usize).max(w / 2);
                let p0 = unsafe { std::slice::from_raw_parts(src.data[0] as *const u8, ls0 * h) };
                let p1 =
                    unsafe { std::slice::from_raw_parts(src.data[1] as *const u8, ls1 * (h / 2)) };
                let p2 =
                    unsafe { std::slice::from_raw_parts(src.data[2] as *const u8, ls2 * (h / 2)) };
                Some(Self {
                    width: src.width,
                    height: src.height,
                    format: VideoFormat::I420,
                    linesize: [ls0, ls1],
                    plane0: p0.to_vec(),
                    plane1: p1.to_vec(),
                    plane2: p2.to_vec(),
                })
            }
            VideoFormat::BGRA | VideoFormat::RGBA => {
                let ls = (src.linesize[0] as usize).max(w * 4);
                let src_bytes =
                    unsafe { std::slice::from_raw_parts(src.data[0] as *const u8, ls * h) };
                let (ew, eh) = (w & !1, h & !1);
                let mut y_plane = vec![16u8; ew * eh];
                let mut uv_plane = vec![128u8; ew * (eh / 2)];
                let is_bgra = src.format == VideoFormat::BGRA;

                for row in 0..eh {
                    let row_off = row * ls;
                    for col in 0..ew {
                        let px = row_off + col * 4;
                        let (r, g, b) = if is_bgra {
                            (
                                src_bytes[px + 2] as i32,
                                src_bytes[px + 1] as i32,
                                src_bytes[px] as i32,
                            )
                        } else {
                            (
                                src_bytes[px] as i32,
                                src_bytes[px + 1] as i32,
                                src_bytes[px + 2] as i32,
                            )
                        };
                        y_plane[row * ew + col] =
                            (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16).clamp(0, 255) as u8;
                        if (row & 1) == 0 && (col & 1) == 0 {
                            let u = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128).clamp(0, 255)
                                as u8;
                            let v = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128).clamp(0, 255)
                                as u8;
                            let uv_idx = (row / 2) * ew + col;
                            uv_plane[uv_idx] = u;
                            uv_plane[uv_idx + 1] = v;
                        }
                    }
                }
                Some(Self {
                    width: ew as u32,
                    height: eh as u32,
                    format: VideoFormat::NV12,
                    linesize: [ew, ew],
                    plane0: y_plane,
                    plane1: uv_plane,
                    plane2: Vec::new(),
                })
            }
        }
    }

    fn as_video_frame(&self) -> VideoFrame {
        VideoFrame {
            format: self.format,
            sub_format: VideoSubFormat::SW,
            width: self.width,
            height: self.height,
            data: [
                self.plane0.as_ptr() as *const c_void,
                self.plane1.as_ptr() as *const c_void,
                if self.plane2.is_empty() {
                    std::ptr::null()
                } else {
                    self.plane2.as_ptr() as *const c_void
                },
            ],
            linesize: [self.linesize[0] as u32, self.linesize[1] as u32, 0],
        }
    }
}

struct PreviewAndMuxVideoConsumer {
    sink: LiveFrameSink,
    mux_tx: mpsc::SyncSender<EncoderMsg>,
    stopped: Arc<AtomicBool>,
}

impl FrameConsumer for PreviewAndMuxVideoConsumer {
    type Frame = VideoFrame;
    fn sink(&mut self, frame: &Self::Frame) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return false;
        }
        let _ = self.sink.video(frame);
        if let Some(owned) = OwnedVideoFrame::from_borrowed(frame) {
            let _ = self.mux_tx.try_send(EncoderMsg::Video(owned));
        }
        true
    }
}

struct PreviewAndMuxAudioConsumer {
    mux_tx: mpsc::SyncSender<EncoderMsg>,
    stopped: Arc<AtomicBool>,
}

impl FrameConsumer for PreviewAndMuxAudioConsumer {
    type Frame = AudioFrame;
    fn sink(&mut self, frame: &Self::Frame) -> bool {
        if self.stopped.load(Ordering::Relaxed) {
            return false;
        }
        if frame.data.is_null() || frame.frames == 0 {
            return true;
        }
        let slice = unsafe { std::slice::from_raw_parts(frame.data, (frame.frames as usize) * 2) };
        let _ = self.mux_tx.try_send(EncoderMsg::Audio(OwnedAudioFrame {
            sample_rate: frame.sample_rate,
            frames: frame.frames,
            pcm_i16: slice.to_vec(),
        }));
        true
    }
}

pub fn resolve_encoder_type(encoder_name: &str) -> VideoEncoderType {
    let low = encoder_name.to_ascii_lowercase();
    #[cfg(target_os = "linux")]
    if low.contains("vaapi") {
        return VideoEncoderType::Vaapi;
    }
    let _ = low;
    VideoEncoderType::X264
}

fn pick_screen_source(
    screens: &[Source],
    preferred_virtual_id: Option<&str>,
    target_monitor: &str,
) -> Source {
    if let Some(pref_id) = preferred_virtual_id {
        if let Some(found) = screens.iter().find(|s| s.id == pref_id) {
            return found.clone();
        }
        if pref_id.starts_with("wayland_") {
            return Source {
                id: pref_id.to_string(),
                index: 0,
                is_default: true,
                kind: SourceType::Screen,
                name: pref_id.trim_start_matches("wayland_").to_string(),
            };
        }
    }

    let clean_target = target_monitor.trim();
    if !clean_target.is_empty()
        && !clean_target.eq_ignore_ascii_case("auto")
        && let Some(found) = screens.iter().find(|s| {
            s.id == clean_target
                || s.id.strip_prefix("wayland_") == Some(clean_target)
                || s.name.contains(clean_target)
        })
    {
        return found.clone();
    }

    screens
        .iter()
        .find(|s| s.is_default)
        .or_else(|| screens.first())
        .cloned()
        .unwrap_or_else(|| Source {
            id: "wayland_default".to_string(),
            index: 0,
            is_default: true,
            kind: SourceType::Screen,
            name: "Primary Display".to_string(),
        })
}

fn pick_audio_source(settings: &AppSettings) -> Option<Source> {
    if !settings.capture_audio {
        return None;
    }
    let audio_sources = Capture::get_sources(SourceType::Audio).unwrap_or_default();
    let pref = settings.audio_source.trim();
    if !pref.is_empty()
        && !pref.eq_ignore_ascii_case("auto")
        && let Some(found) = audio_sources
            .iter()
            .find(|s| s.id == pref || s.name == pref || s.name.contains(pref))
    {
        return Some(found.clone());
    }
    audio_sources
        .iter()
        .find(|s| s.id == "output:pipewire-monitor")
        .or_else(|| audio_sources.iter().find(|s| s.id.starts_with("output:")))
        .or_else(|| audio_sources.iter().find(|s| s.is_default))
        .or_else(|| audio_sources.first())
        .cloned()
}

fn spawn_srt_sender(
    sender_options: NayutacastSenderOptions,
    sink: LiveFrameSink,
) -> anyhow::Result<SrtSenderHandle> {
    let (init_tx, init_rx) =
        mpsc::sync_channel::<Result<common::MediaStreamDescription, String>>(1);
    let client_count = Arc::new(AtomicUsize::new(0));
    let stop_flag = Arc::new(AtomicBool::new(false));
    let count_thr = client_count.clone();
    let stop_thr = stop_flag.clone();

    let join = std::thread::Builder::new()
        .name("NayutaCastSrtOwner".to_string())
        .spawn(
            move || match nayutacast::create_sender(&sender_options, sink, ()) {
                Ok(sender) => {
                    let desc = sender.get_description().clone();
                    let _ = init_tx.send(Ok(desc));
                    while !stop_thr.load(Ordering::SeqCst) {
                        count_thr.store(sender.client_count(), Ordering::SeqCst);
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    drop(sender);
                }
                Err(e) => {
                    let _ = init_tx.send(Err(format!("{:?}", e)));
                }
            },
        )?;

    let desc = init_rx
        .recv_timeout(Duration::from_secs(6))
        .map_err(|_| anyhow::anyhow!("Timeout inisialisasi sender SRT NayutaCast"))?
        .map_err(|e| anyhow::anyhow!("NayutacastSender init error: {}", e))?;

    Ok(SrtSenderHandle {
        description: desc,
        client_count,
        stop_flag,
        join_handle: Mutex::new(Some(join)),
    })
}

fn spawn_encoder_worker(
    mux_opts: LiveMuxerOptions,
    hub: Arc<StreamHub>,
    rt: tokio::runtime::Handle,
    stopped: Arc<AtomicBool>,
    rx: mpsc::Receiver<EncoderMsg>,
) -> anyhow::Result<std::thread::JoinHandle<()>> {
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);
    let join = std::thread::Builder::new()
        .name("UniversalLiveEncoder".to_string())
        .spawn(move || {
            let mut muxer = match UniversalLiveMuxer::new(mux_opts.clone()) {
                Ok(m) => m,
                Err(e) => {
                    log::warn!(
                        "Muxer init {:?} failed ({}); retrying x264",
                        mux_opts.encoder_type,
                        e
                    );
                    let mut fallback = mux_opts.clone();
                    fallback.encoder_type = VideoEncoderType::X264;
                    match UniversalLiveMuxer::new(fallback) {
                        Ok(m) => m,
                        Err(e2) => {
                            let _ = init_tx.send(Err(e2.to_string()));
                            return;
                        }
                    }
                }
            };
            let mut video_epoch: Option<Instant> = None;
            let mut v_frames: u64 = 0;
            let mut mp4_frags: u64 = 0;
            let _ = init_tx.send(Ok(()));

            while !stopped.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(EncoderMsg::Video(vf)) => {
                        let elapsed = match video_epoch {
                            Some(t0) => t0.elapsed(),
                            None => {
                                video_epoch = Some(Instant::now());
                                Duration::ZERO
                            }
                        };
                        let raw = vf.as_video_frame();
                        match muxer.push_video_at(&raw, elapsed) {
                            Ok(out) => {
                                v_frames += 1;
                                if out.mp4_fragment.is_some() {
                                    mp4_frags += 1;
                                    if mp4_frags == 1 {
                                        log::info!(
                                            "UniversalLiveEncoder: first fMP4 fragment ready after {} frames ({:.2}s)",
                                            v_frames,
                                            elapsed.as_secs_f32()
                                        );
                                    }
                                }
                                rt.block_on(hub.sync_from_muxer(&muxer, out));
                            }
                            Err(e) => log::warn!("LiveMuxer push_video error: {}", e),
                        }
                    }
                    Ok(EncoderMsg::Audio(af)) => {
                        if video_epoch.is_none() {
                            continue;
                        }
                        let raw = AudioFrame {
                            sample_rate: af.sample_rate,
                            frames: af.frames,
                            data: af.pcm_i16.as_ptr(),
                        };
                        match muxer.push_audio(&raw) {
                            Ok(out) => rt.block_on(hub.sync_from_muxer(&muxer, out)),
                            Err(e) => log::warn!("LiveMuxer push_audio error: {}", e),
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => continue,
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            if let Some(t0) = video_epoch {
                let secs = t0.elapsed().as_secs_f32().max(0.001);
                log::info!(
                    "UniversalLiveEncoder stopped: {} video frames ({:.1} fps avg), {} fMP4 fragments",
                    v_frames,
                    v_frames as f32 / secs,
                    mp4_frags
                );
            }
        })?;

    init_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|_| anyhow::anyhow!("Timeout inisialisasi muxer encoder"))?
        .map_err(|e| anyhow::anyhow!("Encoder gagal: {}", e))?;

    Ok(join)
}

#[allow(clippy::too_many_arguments)]
fn start_smart_tv_capture(
    source: Source,
    audio_source: Option<Source>,
    width: u32,
    height: u32,
    fps: u8,
    sample_rate: u32,
    sink: LiveFrameSink,
    mux_tx: mpsc::SyncSender<EncoderMsg>,
    stopped: Arc<AtomicBool>,
) -> anyhow::Result<Capture> {
    let audio_opt = audio_source.map(|src| SourceCaptureOptions {
        description: AudioCaptureSourceDescription {
            source: src,
            sample_rate,
        },
        consumer: PreviewAndMuxAudioConsumer {
            mux_tx: mux_tx.clone(),
            stopped: stopped.clone(),
        },
    });

    let opts = CaptureOptions {
        video: Some(SourceCaptureOptions {
            description: VideoCaptureSourceDescription {
                #[cfg(target_os = "windows")]
                direct3d: common::win32::Direct3DDevice::new()
                    .map_err(|e| anyhow::anyhow!("Gagal menginisialisasi Direct3D: {:?}", e))?,
                source,
                size: Size { width, height },
                fps,
                hardware: false,
            },
            consumer: PreviewAndMuxVideoConsumer {
                sink,
                mux_tx,
                stopped,
            },
        }),
        audio: audio_opt,
    };

    Capture::start(opts).map_err(|e| anyhow::anyhow!("Gagal memulai screen capture: {:?}", e))
}

#[derive(Clone)]
struct SmartTvPipelineShared {
    device: DeviceInfo,
    lan_ip: Ipv4Addr,
    hub: Arc<StreamHub>,
    http_port: u16,
    state: Arc<RwLock<CastingState>>,
    backend: Arc<tokio::sync::Mutex<Option<ActiveCastBackend>>>,
}

pub struct CastingSession {
    #[allow(dead_code)]
    pub target: DeviceInfo,
    pub target_name: String,
    #[allow(dead_code)]
    pub target_ip: String,
    pub start_time: Instant,
    pub frame_buffer: Arc<Mutex<Option<ColorImage>>>,
    sink: LiveFrameSink,
    rt: tokio::runtime::Handle,
    device_mgr: Arc<DeviceManager>,
    settings: AppSettings,
    state: Arc<RwLock<CastingState>>,
    cached_state: Mutex<CastingState>,
    stopped: Arc<AtomicBool>,
    virtual_display: Arc<Mutex<Option<VirtualDisplayGuard>>>,
    display_mode: Mutex<DisplayMode>,
    active_capture: Arc<Mutex<Option<Capture>>>,
    mux_tx: Arc<Mutex<Option<mpsc::SyncSender<EncoderMsg>>>>,
    encoder_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    http_server: Arc<Mutex<Option<HttpStreamServer>>>,
    backend: Arc<tokio::sync::Mutex<Option<ActiveCastBackend>>>,
    orchestrator_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pipeline_shared: Arc<Mutex<Option<SmartTvPipelineShared>>>,
}

impl CastingSession {
    pub fn start_device(
        device: DeviceInfo,
        device_mgr: Arc<DeviceManager>,
        settings: &AppSettings,
        display_mode: DisplayMode,
        rt: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let width = settings.resolution_width.max(320) & !1;
        let height = settings.resolution_height.max(240) & !1;
        let fps = settings.frame_rate.max(1);
        let encoder_type = resolve_encoder_type(&settings.video_encoder);

        let lan_ip =
            net_utils::resolve_lan_ip_for_target(Some(device.ip)).unwrap_or(Ipv4Addr::LOCALHOST);

        log::info!(
            "Starting universal casting session to '{}' ({}) via LAN IP {} (mode={:?})",
            device.name,
            device.ip,
            lan_ip,
            display_mode
        );

        let (sink, frame_buffer) = LiveFrameSink::new();
        let initial_state = CastingState::Connecting {
            protocol: "Inisialisasi".to_string(),
            message: format!("Menyiapkan layar & server stream ke {}...", device.name),
        };
        let state = Arc::new(RwLock::new(initial_state.clone()));
        let cached_state = Mutex::new(initial_state);
        let stopped = Arc::new(AtomicBool::new(false));

        let guard = VirtualDisplayGuard::acquire(
            display_mode,
            Some(&settings.target_monitor),
            width,
            height,
            fps,
            settings.create_virtual_display,
        );
        let screens = Capture::get_sources(SourceType::Screen).unwrap_or_default();
        let display_source = pick_screen_source(
            &screens,
            guard.preferred_source_id().as_deref(),
            &settings.target_monitor,
        );
        let audio_source = pick_audio_source(settings);

        let preferred_proto = CastProtocol::from_setting_str(&settings.preferred_protocol);
        let ordered_protocols = device.ordered_protocols(preferred_proto);

        let virtual_display = Arc::new(Mutex::new(Some(guard)));
        let active_capture: Arc<Mutex<Option<Capture>>> = Arc::new(Mutex::new(None));
        let mux_tx_holder: Arc<Mutex<Option<mpsc::SyncSender<EncoderMsg>>>> =
            Arc::new(Mutex::new(None));
        let encoder_thread: Arc<Mutex<Option<std::thread::JoinHandle<()>>>> =
            Arc::new(Mutex::new(None));
        let http_server_holder: Arc<Mutex<Option<HttpStreamServer>>> = Arc::new(Mutex::new(None));
        let backend_holder: Arc<tokio::sync::Mutex<Option<ActiveCastBackend>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let pipeline_shared: Arc<Mutex<Option<SmartTvPipelineShared>>> = Arc::new(Mutex::new(None));

        let target_name = device.name.clone();
        let target_ip = device.ip.to_string();

        // Clone handles into async connection orchestrator.
        let dev_bg = device.clone();
        let dm_bg = device_mgr.clone();
        let settings_bg = settings.clone();
        let sink_bg = sink.clone();
        let state_bg = state.clone();
        let stopped_bg = stopped.clone();
        let cap_bg = active_capture.clone();
        let mux_tx_bg = mux_tx_holder.clone();
        let enc_thr_bg = encoder_thread.clone();
        let srv_bg = http_server_holder.clone();
        let backend_bg = backend_holder.clone();
        let shared_bg = pipeline_shared.clone();
        let rt_bg = rt.clone();

        // Start Smart TV muxer + capture upfront if first protocol isn't
        // exclusively SRT, or spin it up on demand after SRT finishes/fails.
        let starts_with_srt =
            ordered_protocols.first().copied() == Some(CastProtocol::NayutacastNative);

        let orchestrator = rt.spawn(async move {
            let hub = StreamHub::new();
            let http_srv = match HttpStreamServer::start(lan_ip, hub.clone()).await {
                Ok(s) => s,
                Err(_) => match HttpStreamServer::start(Ipv4Addr::UNSPECIFIED, hub.clone()).await {
                    Ok(s) => s,
                    Err(e) => {
                        *state_bg.write().await = CastingState::Failed {
                            error: format!("Gagal menjalankan server HTTP lokal: {}", e),
                            tried_protocols: Vec::new(),
                            browser_url: String::new(),
                        };
                        return;
                    }
                },
            };
            let http_port = http_srv.port;
            let browser_url = browser_watch_url(lan_ip, http_port);
            *srv_bg.lock() = Some(http_srv);

            let shared = SmartTvPipelineShared {
                device: dev_bg.clone(),
                lan_ip,
                hub: hub.clone(),
                http_port,
                state: state_bg.clone(),
                backend: backend_bg.clone(),
            };
            *shared_bg.lock() = Some(shared.clone());

            let mut remaining = ordered_protocols.clone();
            let mut srt_error: Option<String> = None;

            if starts_with_srt {
                remaining.retain(|p| *p != CastProtocol::NayutacastNative);
                let bind_addr: SocketAddr = SocketAddr::new(IpAddr::V4(lan_ip), settings_bg.port);
                let sender_options = NayutacastSenderOptions {
                    transport: TransportOptions {
                        mtu: settings_bg.mtu as usize,
                        strategy: TransportStrategy::Direct(bind_addr),
                    },
                    media: NayutacastSenderMediaOptions {
                        video: Some(NayutacastSenderTrackOptions {
                            source: display_source.clone(),
                            options: VideoOptions {
                                codec: encoder_type,
                                frame_rate: fps,
                                width,
                                height,
                                bit_rate: (settings_bg.video_bitrate_mbps as u64) * 1_000_000,
                                key_frame_interval: 24,
                            },
                        }),
                        audio: audio_source
                            .clone()
                            .map(|src| NayutacastSenderTrackOptions {
                                source: src,
                                options: AudioOptions {
                                    sample_rate: settings_bg.audio_sample_rate as u64,
                                    bit_rate: (settings_bg.audio_bitrate_kbps as u64) * 1_000,
                                },
                            }),
                    },
                };

                match spawn_srt_sender(sender_options, sink_bg.clone()) {
                    Ok(srt_handle) => {
                        match NayutacastCastSession::connect(
                            srt_handle,
                            lan_ip,
                            settings_bg.port,
                            &dev_bg,
                            settings_bg.device_name.clone(),
                            dm_bg.clone(),
                            state_bg.clone(),
                            http_port,
                        )
                        .await
                        {
                            Ok(session) => {
                                *backend_bg.lock().await =
                                    Some(ActiveCastBackend::Nayutacast(session));
                                return;
                            }
                            Err(e) => {
                                srt_error = Some(e.to_string());
                            }
                        }
                    }
                    Err(e) => {
                        srt_error = Some(e.to_string());
                    }
                }
                if stopped_bg.load(Ordering::SeqCst) {
                    return;
                }
            }

            if remaining.is_empty() {
                *state_bg.write().await = CastingState::Failed {
                    error: srt_error.unwrap_or_else(|| "Koneksi NayutaCast SRT gagal".to_string()),
                    tried_protocols: vec!["NayutaCast Native (SRT)".to_string()],
                    browser_url,
                };
                return;
            }

            // Initialize Smart TV Capture + Muxer + HTTP Stream Server pipeline
            let (tx, rx) = mpsc::sync_channel::<EncoderMsg>(16);
            *mux_tx_bg.lock() = Some(tx.clone());
            let mux_opts = LiveMuxerOptions {
                width,
                height,
                fps,
                video_bitrate: (settings_bg.video_bitrate_mbps as u64).clamp(1, 25) * 1_000_000,
                audio_sample_rate: settings_bg.audio_sample_rate,
                audio_bitrate: (settings_bg.audio_bitrate_kbps as u64) * 1_000,
                encoder_type,
            };

            match spawn_encoder_worker(mux_opts, hub.clone(), rt_bg.clone(), stopped_bg.clone(), rx)
            {
                Ok(enc_handle) => {
                    *enc_thr_bg.lock() = Some(enc_handle);
                }
                Err(e) => {
                    *state_bg.write().await = CastingState::Failed {
                        error: e.to_string(),
                        tried_protocols: Vec::new(),
                        browser_url,
                    };
                    return;
                }
            }

            match start_smart_tv_capture(
                display_source,
                audio_source,
                width,
                height,
                fps,
                settings_bg.audio_sample_rate,
                sink_bg,
                tx,
                stopped_bg.clone(),
            ) {
                Ok(cap) => {
                    *cap_bg.lock() = Some(cap);
                }
                Err(e) => {
                    *state_bg.write().await = CastingState::Failed {
                        error: e.to_string(),
                        tried_protocols: Vec::new(),
                        browser_url,
                    };
                    return;
                }
            }

            // Give muxer ~350ms to emit initial ftyp+moov & first GOP segment
            // before TV opens URL.
            tokio::time::sleep(Duration::from_millis(350)).await;
            if stopped_bg.load(Ordering::SeqCst) {
                return;
            }

            let ctx = CastContext {
                device: shared.device,
                lan_ip: shared.lan_ip,
                http_port: shared.http_port,
                hub: shared.hub,
                state: shared.state.clone(),
            };

            match connect_smart_tv_protocols(ctx, &remaining).await {
                Ok(active) => {
                    *backend_bg.lock().await = Some(active);
                }
                Err(e) => {
                    log::warn!("Smart TV protocol orchestration failed: {}", e);
                    if let Some(prev) = srt_error {
                        let mut st = state_bg.write().await;
                        if let CastingState::Failed {
                            error,
                            tried_protocols,
                            ..
                        } = &mut *st
                        {
                            *error = format!("NayutaCast SRT: {} | {}", prev, error);
                            tried_protocols.insert(0, "NayutaCast Native (SRT)".to_string());
                        }
                    }
                }
            }
        });

        Ok(Self {
            target: device,
            target_name,
            target_ip,
            start_time: Instant::now(),
            frame_buffer,
            sink,
            rt,
            device_mgr,
            settings: settings.clone(),
            state,
            cached_state,
            stopped,
            virtual_display,
            display_mode: Mutex::new(display_mode),
            active_capture,
            mux_tx: mux_tx_holder,
            encoder_thread,
            http_server: http_server_holder,
            backend: backend_holder,
            orchestrator_task: Mutex::new(Some(orchestrator)),
            pipeline_shared,
        })
    }

    pub fn start_for_cli(
        target_name: String,
        target_ip: String,
        device_mgr: Arc<DeviceManager>,
        settings: &AppSettings,
        rt: tokio::runtime::Handle,
    ) -> anyhow::Result<Self> {
        let ip: IpAddr = target_ip
            .parse()
            .unwrap_or(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)));
        let dev = DeviceInfo {
            id: format!("ip:{}", ip),
            name: target_name,
            ip,
            port: 8009,
            network: DeviceKind::Wifi,
            manufacturer: None,
            model_name: None,
            protocols: vec![CastProtocol::BrowserFallback],
            av_transport_control_url: None,
            connection_mgr_control_url: None,
            location_url: None,
            cast_port: None,
            airplay_port: None,
            roku_port: None,
            nayutacast_peer_id: None,
            is_custom: true,
        };
        let mode = DisplayMode::from_str_lossy(&settings.display_mode);
        Self::start_device(dev, device_mgr, settings, mode, rt)
    }

    pub fn get_state(&self) -> CastingState {
        if let Ok(guard) = self.state.try_read() {
            let cloned = guard.clone();
            *self.cached_state.lock() = cloned.clone();
            cloned
        } else {
            self.cached_state.lock().clone()
        }
    }

    pub fn display_mode(&self) -> DisplayMode {
        *self.display_mode.lock()
    }

    pub fn virtual_output_name(&self) -> Option<String> {
        self.virtual_display
            .lock()
            .as_ref()
            .and_then(|g| g.virtual_output_name().map(|s| s.to_string()))
    }

    /// Switches Mirror / Extend layout live and updates capture output if
    /// running Smart TV capture.
    pub fn set_display_mode(&mut self, mode: DisplayMode) -> anyhow::Result<()> {
        *self.display_mode.lock() = mode;
        let preferred_source = {
            let mut guard = self.virtual_display.lock();
            if let Some(vd) = guard.as_mut() {
                vd.apply_mode(mode)?;
                vd.preferred_source_id()
            } else {
                None
            }
        };

        // If Smart TV capture pipeline is active, switch captured monitor
        // smoothly.
        let maybe_tx = self.mux_tx.lock().clone();
        if let Some(tx) = maybe_tx {
            let width = self.settings.resolution_width.max(320) & !1;
            let height = self.settings.resolution_height.max(240) & !1;
            let fps = self.settings.frame_rate.max(1);
            let screens = Capture::get_sources(SourceType::Screen).unwrap_or_default();
            let display_source = pick_screen_source(
                &screens,
                preferred_source.as_deref(),
                &self.settings.target_monitor,
            );
            let audio_source = pick_audio_source(&self.settings);

            if let Some(old_cap) = self.active_capture.lock().take() {
                let _ = old_cap.close();
            }
            let new_cap = start_smart_tv_capture(
                display_source,
                audio_source,
                width,
                height,
                fps,
                self.settings.audio_sample_rate,
                self.sink.clone(),
                tx,
                self.stopped.clone(),
            )?;
            *self.active_capture.lock() = Some(new_cap);
        }

        Ok(())
    }

    /// Switches immediately to Browser `/watch` mode while keeping capture &
    /// HTTP stream live.
    pub fn switch_to_browser_mode(&self) {
        let Some(shared) = self.pipeline_shared.lock().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Some(mut old) = shared.backend.lock().await.take() {
                old.stop().await;
            }
            let ctx = CastContext {
                device: shared.device,
                lan_ip: shared.lan_ip,
                http_port: shared.http_port,
                hub: shared.hub,
                state: shared.state,
            };
            let active = start_browser_watch(ctx).await;
            *shared.backend.lock().await = Some(active);
        });
    }

    /// Re-attempts connection trying alternate protocols on the same live
    /// capture pipeline.
    pub fn retry_other_protocols(&self, excluded_labels: Vec<String>) {
        let Some(shared) = self.pipeline_shared.lock().clone() else {
            return;
        };
        let rt = self.rt.clone();
        rt.spawn(async move {
            if let Some(mut old) = shared.backend.lock().await.take() {
                old.stop().await;
            }
            let mut list: Vec<CastProtocol> = [
                CastProtocol::GoogleCast,
                CastProtocol::DlnaAvTransport,
                CastProtocol::AirPlay,
                CastProtocol::RokuEcp,
                CastProtocol::BrowserFallback,
            ]
            .into_iter()
            .filter(|p| !excluded_labels.iter().any(|lbl| lbl == p.short_label()))
            .collect();

            if list.is_empty() {
                list.push(CastProtocol::BrowserFallback);
            }
            let ctx = CastContext {
                device: shared.device,
                lan_ip: shared.lan_ip,
                http_port: shared.http_port,
                hub: shared.hub,
                state: shared.state,
            };
            if let Ok(active) = connect_smart_tv_protocols(ctx, &list).await {
                *shared.backend.lock().await = Some(active);
            }
        });
    }

    pub fn stop(&mut self) {
        if self.stopped.swap(true, Ordering::SeqCst) {
            return;
        }
        log::info!("Stopping casting session to {}", self.target_name);

        if let Some(task) = self.orchestrator_task.lock().take() {
            task.abort();
        }
        if let Some(cap) = self.active_capture.lock().take() {
            let _ = cap.close();
        }
        *self.mux_tx.lock() = None;
        if let Some(h) = self.encoder_thread.lock().take() {
            let _ = h.join();
        }
        let backend_arc = self.backend.clone();
        self.rt.block_on(async move {
            if let Some(mut b) = backend_arc.lock().await.take() {
                b.stop().await;
            }
        });
        if let Some(mut srv) = self.http_server.lock().take() {
            srv.stop();
        }
        if let Some(mut vd) = self.virtual_display.lock().take() {
            vd.release();
        }
        let _ = &self.device_mgr;
    }
}

impl Drop for CastingSession {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_string_mapping_handles_labels() {
        #[cfg(target_os = "linux")]
        assert_eq!(
            resolve_encoder_type("VAAPI (Hardware)"),
            VideoEncoderType::Vaapi
        );
        assert_eq!(
            resolve_encoder_type("x264 (Software CPU)"),
            VideoEncoderType::X264
        );
    }

    #[tokio::test]
    async fn mock_nayutacast_srt_accepts_receiver_and_counts_client() {
        let _ = transport::startup();
        // Bind to an ephemeral free TCP/UDP port then release so SRT binds
        // reliably.
        let sock = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        drop(sock);

        let addr: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), port);
        let opts = TransportOptions {
            mtu: 1500,
            strategy: TransportStrategy::Direct(addr),
        };

        let sender = transport::create_sender(opts).expect("create SRT sender");
        assert_eq!(sender.client_count(), 0);

        let rx_opts = TransportOptions {
            mtu: 1500,
            strategy: TransportStrategy::Direct(addr),
        };
        let receiver = transport::create_mix_receiver("test-rx", rx_opts)
            .expect("receiver connects to SRT sender");

        for _ in 0..20 {
            if sender.client_count() > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(sender.client_count() >= 1, "expected SRT client_count >= 1");

        drop(receiver);
        drop(sender);
    }
}
