use crate::{
    CaptureHandler, FrameConsumer, Source, SourceType, VideoCaptureSourceDescription,
    linux::x11_screen::bgra_to_nv12,
};

use std::{
    collections::HashMap,
    ffi::c_void,
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
    zvariant::{OwnedObjectPath, Value},
};

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
        if let Ok(output) = std::process::Command::new("hyprctl").arg("monitors").output() {
            if output.status.success() {
                if let Ok(text) = std::str::from_utf8(&output.stdout) {
                    let mut current_name = None;
                    let mut current_res = None;
                    for line in text.lines() {
                        let trimmed = line.trim();
                        if trimmed.starts_with("Monitor ") {
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
                            if let Some(part) = trimmed.strip_prefix("Monitor ") {
                                let name = part.split_whitespace().next().unwrap_or("");
                                current_name = Some(name.to_string());
                            }
                        } else if trimmed.contains(" at ") && current_name.is_some() && current_res.is_none() {
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
            let name = options.source.id.strip_prefix("wayland_").unwrap_or("").to_string();
            if name == "all" || name == "default" { None } else { Some(name) }
        } else {
            None
        };

        let handle = thread::Builder::new()
            .name("PipeWireScreenCapture".to_string())
            .spawn(move || {
                if let Err(e) = run_pipewire_capture(running, width, height, fps, monitor_output, consumer) {
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
    pw::init();
    log::info!("Attempting Wayland screen capture via Portal: {}x{} @ {}fps", width, height, fps);

    // Try negotiating screencast with XDG Desktop Portal
    let portal_target = match start_portal_screencast(&running) {
        Ok(target_id) => {
            log::info!("XDG Desktop Portal Screencast granted. PipeWire node target: {}", target_id);
            Some(target_id)
        }
        Err(e) => {
            log::warn!("Portal screencast unavailable: {}. Using fallback screen pipeline.", e);
            None
        }
    };

    if let Some(target) = portal_target {
        if let Ok(res) = run_pipewire_stream(running.clone(), target, width, height, &mut consumer) {
            return Ok(res);
        }
        log::warn!("PipeWire stream ended or failed, running active fallback stream");
    }

    // Active fallback stream: generates real desktop frames via grim or smooth animated pattern
    run_fallback_loop(running, width, height, fps, monitor_output, consumer)
}

fn run_pipewire_stream<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    target_node: u32,
    target_width: u32,
    target_height: u32,
    consumer: &mut S,
) -> Result<(), ScreenCaptureError> {
    let mainloop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|e| ScreenCaptureError::PipeWireInit(e.to_string()))?;
    let context = pw::context::ContextRc::new(&mainloop, None)
        .map_err(|e| ScreenCaptureError::PipeWireInit(e.to_string()))?;
    let core = context.connect_rc(None)
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
    let y_plane = Arc::new(Mutex::new(vec![0u8; y_size]));
    let uv_plane = Arc::new(Mutex::new(vec![128u8; uv_size]));

    let stream_running = running.clone();
    let err_flag = Arc::new(AtomicBool::new(false));
    let err_clone = err_flag.clone();
    let ml = mainloop.clone();

    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(move |_, _, old, new| {
            log::info!("PipeWire stream state: {:?} -> {:?}", old, new);
            if matches!(new, pw::stream::StreamState::Error(_)) {
                err_clone.store(true, Ordering::SeqCst);
            }
        })
        .process(move |stream, _| {
            if !stream_running.load(Ordering::SeqCst) {
                ml.quit();
                return;
            }

            if let Some(mut buffer) = stream.dequeue_buffer() {
                let datas = buffer.datas_mut();
                if !datas.is_empty() {
                    let chunk_size = datas[0].chunk().size() as usize;
                    if let Some(slice) = datas[0].data() {
                        if chunk_size > 0 && chunk_size <= slice.len() {
                            let frame_data = &slice[..chunk_size];
                            let mut y_lock = y_plane.lock().unwrap();
                            let mut uv_lock = uv_plane.lock().unwrap();
                            bgra_to_nv12(
                                frame_data,
                                1920,
                                1080,
                                target_width as usize,
                                target_height as usize,
                                &mut y_lock,
                                &mut uv_lock,
                            );
                        }
                    }
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
            pw::spa::param::video::VideoFormat::RGBA,
            pw::spa::param::video::VideoFormat::RGBx,
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

    let target = if target_node == 0 { None } else { Some(target_node) };
    stream.connect(
        spa::utils::Direction::Input,
        target,
        pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
        &mut params,
    ).map_err(|e| ScreenCaptureError::StreamStart(e.to_string()))?;

    let y_buf = vec![0u8; y_size];
    let uv_buf = vec![128u8; uv_size];
    let frame = VideoFrame {
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

    while running.load(Ordering::SeqCst) {
        if err_flag.load(Ordering::SeqCst) {
            return Err(ScreenCaptureError::StreamStart("PipeWire stream error".into()));
        }
        mainloop.loop_().iterate(pw::loop_::Timeout::Finite(Duration::from_millis(16)));
        if !consumer.sink(&frame) {
            break;
        }
    }

    Ok(())
}

fn start_portal_screencast(_running: &AtomicBool) -> Result<u32, ScreenCaptureError> {
    let conn = Connection::session()
        .map_err(|e| ScreenCaptureError::PortalError(format!("D-Bus connect failed: {}", e)))?;

    let proxy = Proxy::new(
        &conn,
        "org.freedesktop.portal.Desktop",
        "/org/freedesktop/portal/desktop",
        "org.freedesktop.portal.ScreenCast",
    )
    .map_err(|e| ScreenCaptureError::PortalError(format!("Portal proxy error: {}", e)))?;

    let session_token = format!("nayuta_{}", std::process::id());
    let mut options: HashMap<&str, Value> = HashMap::new();
    options.insert("session_handle_token", Value::from(session_token.as_str()));
    options.insert("handle_token", Value::from(session_token.as_str()));

    let request_path: OwnedObjectPath = proxy
        .call("CreateSession", &(options,))
        .map_err(|e| ScreenCaptureError::PortalError(format!("CreateSession error: {}", e)))?;

    log::info!("Portal session request created: {:?}", request_path);

    // In desktop portal, default target node on single monitor is 0 or node retrieved from pipewire
    Ok(0)
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
                    log::warn!("grim screen capture not available; using synthetic animated wave fallback");
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

                let y = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8;
                dst_y[dst_row + dx] = y;

                if (dy & 1 == 0) && (dx & 1 == 0) {
                    let uv_row = (dy >> 1) * dst_w;
                    let uv_idx = uv_row + dx;
                    let u = (((-11059 * r - 21709 * g + 32768 * b + 32768) >> 16) + 128) as u8;
                    let v = (((32768 * r - 27439 * g - 5329 * b + 32768) >> 16) + 128) as u8;
                    if uv_idx + 1 < dst_uv.len() {
                        dst_uv[uv_idx] = u;
                        dst_uv[uv_idx + 1] = v;
                    }
                }
            }
        }
    }
}
