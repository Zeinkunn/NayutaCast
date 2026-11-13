use crate::{CaptureHandler, FrameConsumer, Source, SourceType, VideoCaptureSourceDescription};

use std::{
    collections::HashMap,
    ffi::c_void,
    os::fd::OwnedFd,
    ptr::null,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::frame::{VideoFormat, VideoFrame, VideoSubFormat};
use pipewire as pw;
use pw::spa::{self, pod::Pod};
use thiserror::Error;
use zbus::{
    blocking::{Connection, Proxy},
    zvariant::{OwnedFd as ZbusOwnedFd, OwnedObjectPath, OwnedValue, Value},
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PixelOrder {
    Bgrx,
    Rgbx,
}

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ScreenCaptureError {
    #[error("PipeWire initialization failed: {0}")]
    PipeWireInit(String),
    #[error("No screen source available")]
    NoScreenSource,
    #[error("Failed to start stream: {0}")]
    StreamStart(String),
    #[error("Portal error: {0}")]
    PortalError(String),
    #[error("Thread creation error: {0}")]
    ThreadError(#[from] std::io::Error),
}

pub struct PipeWireScreenCapture {
    running: Arc<AtomicBool>,
    thread_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl Default for PipeWireScreenCapture {
    fn default() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            thread_handle: Arc::new(Mutex::new(None)),
        }
    }
}

impl CaptureHandler for PipeWireScreenCapture {
    type Frame = VideoFrame;
    type Error = ScreenCaptureError;
    type CaptureOptions = VideoCaptureSourceDescription;

    fn get_sources() -> Result<Vec<Source>, Self::Error> {
        let mut sources = Vec::new();
        // Detect monitors via hyprctl monitors if on Hyprland / Wayland
        if let Ok(output) = std::process::Command::new("hyprctl")
            .arg("monitors")
            .output()
        {
            if output.status.success() {
                if let Ok(text) = std::str::from_utf8(&output.stdout) {
                    let mut current_name = None;
                    let mut current_res = None;
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("Monitor ") {
                            if let (Some(name), Some(res)) =
                                (current_name.take(), current_res.take())
                            {
                                let idx = sources.len();
                                sources.push(Source {
                                    id: format!("wayland_{}", name),
                                    name: format!("{} ({})", name, res),
                                    index: idx,
                                    kind: SourceType::Screen,
                                    is_default: idx == 0,
                                });
                            }
                            if let Some(part) = trimmed.strip_prefix("Monitor ") {
                                let name = part.split_whitespace().next().unwrap_or("");
                                current_name = Some(name.to_string());
                            }
                        } else if trimmed.contains(" at ")
                            && current_name.is_some()
                            && current_res.is_none()
                        {
                            let res = trimmed.split('@').next().unwrap_or("").trim();
                            current_res = Some(res.to_string());
                        }
                    }
                    if let (Some(name), Some(res)) = (current_name.take(), current_res.take()) {
                        let idx = sources.len();
                        sources.push(Source {
                            id: format!("wayland_{}", name),
                            name: format!("{} ({})", name, res),
                            index: idx,
                            kind: SourceType::Screen,
                            is_default: idx == 0,
                        });
                    }
                }
            }
        }

        if sources.is_empty() {
            sources.push(Source {
                id: "wayland_default".to_string(),
                name: "Primary Monitor (Wayland)".to_string(),
                index: 0,
                kind: SourceType::Screen,
                is_default: true,
            });
        }
        Ok(sources)
    }

    fn start<S: FrameConsumer<Frame = Self::Frame> + 'static>(
        &self,
        options: Self::CaptureOptions,
        consumer: S,
    ) -> Result<(), Self::Error> {
        if self.running.load(Ordering::SeqCst) {
            return Ok(());
        }

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();

        let width = options.size.width;
        let height = options.size.height;
        let fps = options.fps.max(1);

        let monitor_output = if options.source.id.starts_with("wayland_") {
            let name = options
                .source
                .id
                .strip_prefix("wayland_")
                .unwrap_or("")
                .to_string();
            if name == "all" || name == "default" {
                None
            } else {
                Some(name)
            }
        } else {
            None
        };

        let handle = thread::Builder::new()
            .name("PipeWireScreenCapture".to_string())
            .spawn(move || {
                if let Err(e) =
                    run_pipewire_capture(running, width, height, fps, monitor_output, consumer)
                {
                    log::error!("PipeWire capture error: {:?}", e);
                }
            })?;

        *self.thread_handle.lock().unwrap() = Some(handle);
        Ok(())
    }

    fn stop(&self) -> Result<(), Self::Error> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(handle) = self.thread_handle.lock().unwrap().take() {
            let _ = handle.join();
        }
        Ok(())
    }
}

fn run_pipewire_capture<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    width: u32,
    height: u32,
    fps: u8,
    monitor_output: Option<String>,
    mut consumer: S,
) -> Result<(), ScreenCaptureError> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        log::info!(
            "Attempting Wayland wlr-screencopy capture ({:?}) -> {}x{} @ {}fps",
            monitor_output,
            width,
            height,
            fps
        );
        match super::wlr_screencopy::run_wlr_screencopy(
            &running,
            width,
            height,
            fps,
            monitor_output.as_deref(),
            &mut consumer,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => {
                if !running.load(Ordering::SeqCst) {
                    return Ok(());
                }
                log::warn!(
                    "wlr-screencopy unavailable ({}); falling back to XDG Portal + PipeWire",
                    e
                );
            }
        }
    }

    pw::init();
    log::info!(
        "Attempting Wayland screen capture via Portal: {}x{} @ {}fps",
        width,
        height,
        fps
    );

    // Keep portal session connection alive for the lifetime of the PipeWire stream.
    let portal = match start_portal_screencast(&running) {
        Ok(session) => {
            log::info!(
                "XDG Desktop Portal Screencast granted. PipeWire node target: {}",
                session.node_id
            );
            Some(session)
        }
        Err(e) => {
            log::warn!(
                "Portal screencast unavailable: {}. Using grim fallback screen pipeline.",
                e
            );
            None
        }
    };

    if let Some(session) = portal {
        if let Err(e) = run_pipewire_stream(
            running.clone(),
            session.node_id,
            session.pw_fd,
            width,
            height,
            fps,
            &mut consumer,
        ) {
            log::warn!(
                "PipeWire portal stream ended/failed ({}); trying fallback",
                e
            );
        } else {
            drop(session._conn);
            return Ok(());
        }
    }

    if !running.load(Ordering::SeqCst) {
        return Ok(());
    }

    // Active fallback stream: captures via grim on Wayland or synthetic test pattern
    run_fallback_loop(running, width, height, fps, monitor_output, consumer)
}

struct PortalScreencastSession {
    _conn: Connection,
    node_id: u32,
    pw_fd: Option<OwnedFd>,
}

struct PipeWireStreamData {
    format: spa::param::video::VideoInfoRaw,
    order: PixelOrder,
    width: u32,
    height: u32,
    has_format: bool,
}

fn run_pipewire_stream<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    target_node: u32,
    pw_fd: Option<OwnedFd>,
    target_width: u32,
    target_height: u32,
    fps: u8,
    consumer: &mut S,
) -> Result<(), ScreenCaptureError> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| ScreenCaptureError::PipeWireInit(e.to_string()))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| ScreenCaptureError::PipeWireInit(e.to_string()))?;
    let core = match pw_fd {
        Some(fd) => context.connect_fd_rc(fd, None),
        None => context.connect_rc(None),
    }
    .map_err(|e| ScreenCaptureError::PipeWireInit(e.to_string()))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "nayutacast-screen",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| ScreenCaptureError::StreamStart(e.to_string()))?;

    let y_size = (target_width * target_height) as usize;
    let uv_size = (target_width * (target_height / 2)) as usize;
    let shared_planes = Arc::new(Mutex::new((vec![16u8; y_size], vec![128u8; uv_size], 0u64)));
    let planes_for_cb = shared_planes.clone();

    let stream_running = running.clone();
    let err_flag = Arc::new(AtomicBool::new(false));
    let err_clone = err_flag.clone();
    let ml = mainloop.clone();

    let stream_data = PipeWireStreamData {
        format: Default::default(),
        order: PixelOrder::Bgrx,
        width: target_width,
        height: target_height,
        has_format: false,
    };

    let _listener = stream
        .add_local_listener_with_user_data(stream_data)
        .state_changed(move |_, _, old, new| {
            log::info!("PipeWire stream state: {:?} -> {:?}", old, new);
            if matches!(new, pw::stream::StreamState::Error(_)) {
                err_clone.store(true, Ordering::SeqCst);
            }
        })
        .param_changed(move |_, user_data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != pw::spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = pw::spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != pw::spa::param::format::MediaType::Video
                || media_subtype != pw::spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            if user_data.format.parse(param).is_ok() {
                let sz = user_data.format.size();
                if sz.width > 0 && sz.height > 0 {
                    user_data.width = sz.width;
                    user_data.height = sz.height;
                    user_data.has_format = true;
                }
                user_data.order = match user_data.format.format() {
                    pw::spa::param::video::VideoFormat::RGBA
                    | pw::spa::param::video::VideoFormat::RGBx => PixelOrder::Rgbx,
                    _ => PixelOrder::Bgrx,
                };
                log::info!(
                    "PipeWire negotiated format {:?} ({}x{})",
                    user_data.format.format(),
                    user_data.width,
                    user_data.height
                );
            }
        })
        .process(move |stream, user_data| {
            if !stream_running.load(Ordering::SeqCst) {
                ml.quit();
                return;
            }

            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if datas.is_empty() || !user_data.has_format {
                    return;
                }
                let chunk = datas[0].chunk();
                let chunk_offset = chunk.offset() as usize;
                let chunk_size = chunk.size() as usize;
                let chunk_stride = chunk.stride().max(0) as usize;
                let src_w = user_data.width.max(1) as usize;
                let src_h = user_data.height.max(1) as usize;
                let stride = if chunk_stride >= src_w * 4 {
                    chunk_stride
                } else {
                    src_w * 4
                };

                if let Some(slice) = datas[0].data()
                    && chunk_size > 0
                    && chunk_offset.saturating_add(chunk_size) <= slice.len()
                {
                    let frame_data = &slice[chunk_offset..chunk_offset + chunk_size];
                    let mut guard = planes_for_cb.lock().unwrap();
                    let (y_lock, uv_lock, seq) = &mut *guard;
                    packed_4byte_to_nv12(
                        frame_data,
                        src_w,
                        src_h,
                        stride,
                        user_data.order,
                        false,
                        target_width as usize,
                        target_height as usize,
                        y_lock,
                        uv_lock,
                    );
                    *seq = seq.wrapping_add(1);
                }
            }
        })
        .register()
        .map_err(|e| ScreenCaptureError::StreamStart(e.to_string()))?;

    let obj = pw::spa::pod::object!(
        pw::spa::utils::SpaTypes::ObjectParamFormat,
        pw::spa::param::ParamType::EnumFormat,
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaType,
            Id,
            pw::spa::param::format::MediaType::Video
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::MediaSubtype,
            Id,
            pw::spa::param::format::MediaSubtype::Raw
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRx,
            pw::spa::param::video::VideoFormat::BGRA,
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            pw::spa::utils::Rectangle {
                width: target_width,
                height: target_height
            },
            pw::spa::utils::Rectangle {
                width: 1,
                height: 1
            },
            pw::spa::utils::Rectangle {
                width: 7680,
                height: 4320
            }
        ),
        pw::spa::pod::property!(
            pw::spa::param::format::FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            pw::spa::utils::Fraction {
                num: fps as u32,
                denom: 1
            },
            pw::spa::utils::Fraction { num: 0, denom: 1 },
            pw::spa::utils::Fraction { num: 120, denom: 1 }
        ),
    );

    let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(obj),
    )
    .unwrap()
    .0
    .into_inner();

    let mut params = [Pod::from_bytes(&values).unwrap()];

    let target = if target_node == 0 {
        None
    } else {
        Some(target_node)
    };
    stream
        .connect(
            spa::utils::Direction::Input,
            target,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| ScreenCaptureError::StreamStart(e.to_string()))?;

    let mut y_buf = vec![16u8; y_size];
    let mut uv_buf = vec![128u8; uv_size];
    let mut last_seq = 0u64;
    let mut frame = VideoFrame {
        format: VideoFormat::NV12,
        sub_format: VideoSubFormat::SW,
        width: target_width,
        height: target_height,
        data: [
            y_buf.as_ptr() as *const c_void,
            uv_buf.as_ptr() as *const c_void,
            null(),
        ],
        linesize: [target_width, target_width, 0],
    };

    let frame_wait_ms = (1000 / fps.max(1) as u64).clamp(8, 33);
    while running.load(Ordering::SeqCst) {
        if err_flag.load(Ordering::SeqCst) {
            return Err(ScreenCaptureError::StreamStart(
                "PipeWire stream error".into(),
            ));
        }
        mainloop
            .loop_()
            .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(
                frame_wait_ms,
            )));

        let updated = {
            let guard = shared_planes.lock().unwrap();
            if guard.2 != last_seq {
                y_buf.copy_from_slice(&guard.0);
                uv_buf.copy_from_slice(&guard.1);
                last_seq = guard.2;
                true
            } else {
                false
            }
        };

        if updated {
            frame.data[0] = y_buf.as_ptr() as *const c_void;
            frame.data[1] = uv_buf.as_ptr() as *const c_void;
            if !consumer.sink(&frame) {
                break;
            }
        }
    }

    Ok(())
}

fn portal_request_signal(
    conn: &Connection,
    handle_token: &str,
) -> Result<zbus::blocking::proxy::SignalIterator<'static>, ScreenCaptureError> {
    let sender = conn
        .unique_name()
        .ok_or_else(|| ScreenCaptureError::PortalError("Missing D-Bus unique name".into()))?;
    let sanitized = sender.as_str().trim_start_matches(':').replace('.', "_");
    let expected_path = format!(
        "/org/freedesktop/portal/desktop/request/{}/{}",
        sanitized, handle_token
    );
    let req_proxy = Proxy::new(
        conn,
        "org.freedesktop.portal.Desktop",
        expected_path,
        "org.freedesktop.portal.Request",
    )
    .map_err(|e| ScreenCaptureError::PortalError(format!("Request proxy failed: {}", e)))?;

    req_proxy
        .receive_signal("Response")
        .map_err(|e| ScreenCaptureError::PortalError(format!("Signal subscribe failed: {}", e)))
}

fn wait_portal_response(
    signals: &mut zbus::blocking::proxy::SignalIterator<'_>,
) -> Result<HashMap<String, OwnedValue>, ScreenCaptureError> {
    let msg = signals
        .next()
        .ok_or_else(|| ScreenCaptureError::PortalError("Portal response channel closed".into()))?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg
        .body()
        .deserialize()
        .map_err(|e| ScreenCaptureError::PortalError(format!("Portal response decode: {}", e)))?;
    if code != 0 {
        return Err(ScreenCaptureError::PortalError(format!(
            "Portal request cancelled/failed (code {})",
            code
        )));
    }
    Ok(results)
}

fn start_portal_screencast(
    running: &AtomicBool,
) -> Result<PortalScreencastSession, ScreenCaptureError> {
    let conn = Connection::session()
        .map_err(|e| ScreenCaptureError::PortalError(format!("D-Bus connect failed: {}", e)))?;

    let proxy = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .map_err(|e| ScreenCaptureError::PortalError(format!("Portal proxy error: {}", e)))?;

    let pid = std::process::id();
    let create_token = format!("nayuta_create_{}", pid);
    let mut create_sig = portal_request_signal(&conn, &create_token)?;

    let mut create_opts: HashMap<&str, Value> = HashMap::new();
    create_opts.insert(
        "session_handle_token",
        Value::from(format!("nayuta_sess_{}", pid)),
    );
    create_opts.insert("handle_token", Value::from(create_token.as_str()));

    let _req_path: OwnedObjectPath = proxy
        .call("CreateSession", &(create_opts,))
        .map_err(|e| ScreenCaptureError::PortalError(format!("CreateSession error: {}", e)))?;

    let create_res = wait_portal_response(&mut create_sig)?;
    let session_handle_str: String = create_res
        .get("session_handle")
        .and_then(|v| {
            v.downcast_ref::<zbus::zvariant::Str>()
                .ok()
                .map(|s| s.to_string())
        })
        .ok_or_else(|| ScreenCaptureError::PortalError("Missing session_handle".into()))?;
    let session_obj = OwnedObjectPath::try_from(session_handle_str)
        .map_err(|e| ScreenCaptureError::PortalError(format!("Invalid session path: {}", e)))?;

    if !running.load(Ordering::SeqCst) {
        return Err(ScreenCaptureError::PortalError("Stopped".into()));
    }

    let select_token = format!("nayuta_select_{}", pid);
    let mut select_sig = portal_request_signal(&conn, &select_token)?;
    let mut select_opts: HashMap<&str, Value> = HashMap::new();
    select_opts.insert("handle_token", Value::from(select_token.as_str()));
    select_opts.insert("types", Value::from(1u32)); // MONITOR = 1
    select_opts.insert("multiple", Value::from(false));
    select_opts.insert("cursor_mode", Value::from(2u32)); // Embedded = 2
    select_opts.insert("persist_mode", Value::from(2u32)); // Persist until revoked

    let _sel_req: OwnedObjectPath = proxy
        .call("SelectSources", &(&session_obj, select_opts))
        .map_err(|e| ScreenCaptureError::PortalError(format!("SelectSources error: {}", e)))?;
    let _ = wait_portal_response(&mut select_sig)?;

    if !running.load(Ordering::SeqCst) {
        return Err(ScreenCaptureError::PortalError("Stopped".into()));
    }

    let start_token = format!("nayuta_start_{}", pid);
    let mut start_sig = portal_request_signal(&conn, &start_token)?;
    let mut start_opts: HashMap<&str, Value> = HashMap::new();
    start_opts.insert("handle_token", Value::from(start_token.as_str()));

    let _start_req: OwnedObjectPath = proxy
        .call("Start", &(&session_obj, "", start_opts))
        .map_err(|e| ScreenCaptureError::PortalError(format!("Portal Start error: {}", e)))?;
    let start_res = wait_portal_response(&mut start_sig)?;

    let streams_val = start_res.get("streams").ok_or_else(|| {
        ScreenCaptureError::PortalError("Portal Start returned no streams".into())
    })?;
    let streams: Vec<(u32, HashMap<String, OwnedValue>)> = streams_val
        .clone()
        .try_into()
        .map_err(|e| ScreenCaptureError::PortalError(format!("Parse streams error: {:?}", e)))?;
    let node_id = streams
        .first()
        .map(|(id, _)| *id)
        .ok_or_else(|| ScreenCaptureError::PortalError("Empty portal stream list".into()))?;

    let empty_opts: HashMap<&str, Value> = HashMap::new();
    let pw_zbus_fd: ZbusOwnedFd = proxy
        .call("OpenPipeWireRemote", &(&session_obj, empty_opts))
        .map_err(|e| ScreenCaptureError::PortalError(format!("OpenPipeWireRemote error: {}", e)))?;
    let pw_fd: OwnedFd = pw_zbus_fd.into();

    Ok(PortalScreencastSession {
        _conn: conn,
        node_id,
        pw_fd: Some(pw_fd),
    })
}

fn run_fallback_loop<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    width: u32,
    height: u32,
    fps: u8,
    monitor_output: Option<String>,
    mut consumer: S,
) -> Result<(), ScreenCaptureError> {
    let y_size = (width * height) as usize;
    let uv_size = (width * (height / 2)) as usize;
    let mut y_plane = vec![128u8; y_size];
    let mut uv_plane = vec![128u8; uv_size];

    let mut frame = VideoFrame {
        format: VideoFormat::NV12,
        sub_format: VideoSubFormat::SW,
        width,
        height,
        data: [
            y_plane.as_ptr() as *const c_void,
            uv_plane.as_ptr() as *const c_void,
            null(),
        ],
        linesize: [width, width, 0],
    };

    let mut tick = 0u64;
    let frame_duration = Duration::from_millis(1000 / fps as u64);
    let mut grim_active = true;

    while running.load(Ordering::SeqCst) {
        let start = Instant::now();
        tick += 1;

        let mut captured = false;
        if grim_active {
            if let Some((rgb, src_w, src_h)) = capture_grim_frame(monitor_output.as_deref()) {
                rgb_to_nv12(
                    &rgb,
                    src_w,
                    src_h,
                    width as usize,
                    height as usize,
                    &mut y_plane,
                    &mut uv_plane,
                );
                captured = true;
                if tick == 1 {
                    log::info!(
                        "Wayland live desktop capture active via grim ({}x{} -> {}x{})",
                        src_w,
                        src_h,
                        width,
                        height
                    );
                }
            } else {
                if tick == 1 {
                    log::warn!(
                        "grim screen capture not available; using synthetic animated wave fallback"
                    );
                }
                grim_active = false;
            }
        }

        if !captured {
            let offset = ((tick * 4) % width as u64) as usize;
            for y in 0..height as usize {
                let row_idx = y * width as usize;
                let y_val = ((y * 255) / height.max(1) as usize) as u8;
                for x in 0..width as usize {
                    let x_wave = (((x + offset) % 256) as u8) / 2;
                    y_plane[row_idx + x] = y_val / 2 + x_wave;
                }
            }

            let u_val = ((tick * 2) % 256) as u8;
            let v_val = (((tick * 3) + 128) % 256) as u8;
            for i in (0..uv_size).step_by(2) {
                uv_plane[i] = u_val;
                uv_plane[i + 1] = v_val;
            }
        }

        frame.data[0] = y_plane.as_ptr() as *const c_void;
        frame.data[1] = uv_plane.as_ptr() as *const c_void;

        if !consumer.sink(&frame) {
            break;
        }

        let elapsed = start.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
    }

    Ok(())
}

fn capture_grim_frame(output_name: Option<&str>) -> Option<(Vec<u8>, usize, usize)> {
    let mut cmd = std::process::Command::new("grim");
    if let Some(out) = output_name {
        cmd.args(["-o", out]);
    }
    cmd.args(["-t", "ppm", "-"]);
    let output = cmd.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let data = output.stdout;
    if !data.starts_with(b"P6\n") {
        return None;
    }
    let idx = 3;
    let next_newline = data[idx..].iter().position(|&b| b == b'\n')? + idx;
    let dims_str = std::str::from_utf8(&data[idx..next_newline]).ok()?;
    let mut parts = dims_str.split_whitespace();
    let w: usize = parts.next()?.parse().ok()?;
    let h: usize = parts.next()?.parse().ok()?;
    let idx2 = next_newline + 1;
    let maxval_newline = data[idx2..].iter().position(|&b| b == b'\n')? + idx2;
    let rgb = data[maxval_newline + 1..].to_vec();
    if rgb.len() < w * h * 3 {
        return None;
    }
    Some((rgb, w, h))
}

#[inline]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    (((19595 * r + 38470 * g + 7471 * b + 32768) >> 16).clamp(0, 255)) as u8
}

#[inline]
fn rgb_to_uv(r: i32, g: i32, b: i32) -> (u8, u8) {
    let u = ((((-11059 * r - 21709 * g + 32768 * b + 32768) >> 16) + 128).clamp(0, 255)) as u8;
    let v = ((((32768 * r - 27439 * g - 5329 * b + 32768) >> 16) + 128).clamp(0, 255)) as u8;
    (u, v)
}

/// Converts packed 32-bit BGRX/BGRA/RGBX/RGBA buffers (arbitrary stride & optional Y-invert) into NV12.
#[allow(clippy::too_many_arguments)]
pub fn packed_4byte_to_nv12(
    src: &[u8],
    src_w: usize,
    src_h: usize,
    src_stride: usize,
    order: PixelOrder,
    y_invert: bool,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }
    let stride = src_stride.max(src_w * 4);
    let x_ratio = (src_w << 16) / dst_w;
    let y_ratio = (src_h << 16) / dst_h;

    for dy in 0..dst_h {
        let sy_raw = ((dy * y_ratio) >> 16).min(src_h - 1);
        let sy = if y_invert { src_h - 1 - sy_raw } else { sy_raw };
        let src_row = sy * stride;
        let dst_row = dy * dst_w;
        if dst_row + dst_w > dst_y.len() {
            break;
        }

        for dx in 0..dst_w {
            let sx = ((dx * x_ratio) >> 16).min(src_w - 1);
            let px = src_row + sx * 4;
            if px + 3 >= src.len() {
                continue;
            }
            let (r, g, b) = match order {
                PixelOrder::Bgrx => (src[px + 2] as i32, src[px + 1] as i32, src[px] as i32),
                PixelOrder::Rgbx => (src[px] as i32, src[px + 1] as i32, src[px + 2] as i32),
            };
            dst_y[dst_row + dx] = rgb_to_y(r, g, b);
            if (dy & 1 == 0) && (dx & 1 == 0) {
                let uv_idx = (dy >> 1) * dst_w + dx;
                if uv_idx + 1 < dst_uv.len() {
                    let (u, v) = rgb_to_uv(r, g, b);
                    dst_uv[uv_idx] = u;
                    dst_uv[uv_idx + 1] = v;
                }
            }
        }
    }
}

pub fn rgb_to_nv12(
    src_rgb: &[u8],
    src_w: usize,
    src_h: usize,
    dst_w: usize,
    dst_h: usize,
    dst_y: &mut [u8],
    dst_uv: &mut [u8],
) {
    let x_ratio = (src_w << 16) / dst_w.max(1);
    let y_ratio = (src_h << 16) / dst_h.max(1);

    for dy in 0..dst_h {
        let sy = ((dy * y_ratio) >> 16).min(src_h.saturating_sub(1));
        let src_row = sy * src_w * 3;
        let dst_row = dy * dst_w;

        for dx in 0..dst_w {
            let sx = ((dx * x_ratio) >> 16).min(src_w.saturating_sub(1));
            let px = src_row + sx * 3;

            if px + 2 < src_rgb.len() {
                let r = src_rgb[px] as i32;
                let g = src_rgb[px + 1] as i32;
                let b = src_rgb[px + 2] as i32;

                dst_y[dst_row + dx] = rgb_to_y(r, g, b);

                if (dy & 1 == 0) && (dx & 1 == 0) {
                    let uv_row = (dy >> 1) * dst_w;
                    let uv_idx = uv_row + dx;
                    if uv_idx + 1 < dst_uv.len() {
                        let (u, v) = rgb_to_uv(r, g, b);
                        dst_uv[uv_idx] = u;
                        dst_uv[uv_idx + 1] = v;
                    }
                }
            }
        }
    }
}
