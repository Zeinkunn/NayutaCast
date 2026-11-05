use crate::{CaptureHandler, FrameConsumer, Source, SourceType, VideoCaptureSourceDescription};

use std::{
    ffi::c_void,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use common::frame::{VideoFormat, VideoFrame, VideoSubFormat};
use thiserror::Error;
use x11rb::{
    connection::{Connection, RequestConnection},
    protocol::{
        randr::ConnectionExt as RandrExt,
        shm::{self, ConnectionExt as ShmExt},
        xproto::{ConnectionExt as XprotoExt, ImageFormat},
    },
};

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum ScreenCaptureError {
    #[error("X11 connection failed: {0}")]
    X11Connection(String),
    #[error("No screen available")]
    NoScreen,
    #[error("Failed to capture frame: {0}")]
    CaptureFailed(String),
    #[error("Thread creation error: {0}")]
    ThreadError(#[from] std::io::Error),
    #[error("Failed to get display")]
    NoDisplay,
}

pub struct X11ScreenCapture {
    running: Arc<AtomicBool>,
    thread_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl Default for X11ScreenCapture {
    fn default() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            thread_handle: Arc::new(Mutex::new(None)),
        }
    }
}

impl CaptureHandler for X11ScreenCapture {
    type Frame = VideoFrame;
    type Error = ScreenCaptureError;
    type CaptureOptions = VideoCaptureSourceDescription;

    fn get_sources() -> Result<Vec<Source>, Self::Error> {
        let (conn, screen_num) = x11rb::connect(None)
            .map_err(|e| ScreenCaptureError::X11Connection(e.to_string()))?;

        let setup = conn.setup();
        let screen = setup
            .roots
            .get(screen_num)
            .ok_or(ScreenCaptureError::NoScreen)?;

        let mut sources = Vec::new();

        // Enumerate via XRandR if supported
        if let Ok(reply) = conn.randr_get_screen_resources_current(screen.root) {
            if let Ok(res) = reply.reply() {
                for (idx, &crtc) in res.crtcs.iter().enumerate() {
                    if let Ok(crtc_info) = conn.randr_get_crtc_info(crtc, res.config_timestamp) {
                        if let Ok(info) = crtc_info.reply() {
                            if info.width > 0 && info.height > 0 {
                                sources.push(Source {
                                    id: format!("x11_crtc_{}", crtc),
                                    name: format!("Screen {} ({}x{})", idx, info.width, info.height),
                                    index: idx,
                                    kind: SourceType::Screen,
                                    is_default: idx == 0,
                                });
                            }
                        }
                    }
                }
            }
        }

        if sources.is_empty() {
            sources.push(Source {
                id: "x11_screen_0".to_string(),
                name: format!("Default Screen ({}x{})", screen.width_in_pixels, screen.height_in_pixels),
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

        let handle = thread::Builder::new()
            .name("X11ScreenCapture".to_string())
            .spawn(move || {
                if let Err(e) = run_x11_capture(running, options, consumer) {
                    log::error!("X11 capture error: {:?}", e);
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

fn run_x11_capture<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    options: VideoCaptureSourceDescription,
    mut consumer: S,
) -> Result<(), ScreenCaptureError> {
    let (conn, screen_num) = x11rb::connect(None)
        .map_err(|e| ScreenCaptureError::X11Connection(e.to_string()))?;

    let setup = conn.setup();
    let screen = setup
        .roots
        .get(screen_num)
        .ok_or(ScreenCaptureError::NoScreen)?;

    let target_width = options.size.width;
    let target_height = options.size.height;
    let src_width = screen.width_in_pixels as u16;
    let src_height = screen.height_in_pixels as u16;

    log::info!(
        "X11 screen capture starting: src={}x{}, target={}x{}",
        src_width,
        src_height,
        target_width,
        target_height
    );

    // Check SHM extension
    let shm_supported = conn
        .extension_information(shm::X11_EXTENSION_NAME)
        .ok()
        .flatten()
        .is_some();

    let byte_size = (src_width as usize) * (src_height as usize) * 4;
    let mut shm_info = None;

    if shm_supported {
        unsafe {
            let shmid = libc::shmget(
                libc::IPC_PRIVATE,
                byte_size,
                libc::IPC_CREAT | 0o777,
            );
            if shmid >= 0 {
                let shmaddr = libc::shmat(shmid, null(), 0);
                if shmaddr != libc::MAP_FAILED as *mut c_void {
                    if let Ok(seg) = conn.generate_id() {
                        if conn.shm_attach(seg, shmid as u32, false).is_ok() {
                            let _ = conn.flush();
                            shm_info = Some((seg, shmid, shmaddr as *mut u8));
                            log::info!("X11 MIT-SHM capture enabled (shmid={})", shmid);
                        } else {
                            libc::shmdt(shmaddr);
                            libc::shmctl(shmid, libc::IPC_RMID, null_mut());
                        }
                    } else {
                        libc::shmdt(shmaddr);
                        libc::shmctl(shmid, libc::IPC_RMID, null_mut());
                    }
                } else {
                    libc::shmctl(shmid, libc::IPC_RMID, null_mut());
                }
            }
        }
    }

    let y_size = (target_width * target_height) as usize;
    let uv_size = (target_width * (target_height / 2)) as usize;
    let mut y_plane = vec![0u8; y_size];
    let mut uv_plane = vec![128u8; uv_size];

    let mut frame = VideoFrame {
        format: VideoFormat::NV12,
        sub_format: VideoSubFormat::SW,
        width: target_width,
        height: target_height,
        data: [
            y_plane.as_ptr() as *const c_void,
            uv_plane.as_ptr() as *const c_void,
            null(),
        ],
        linesize: [target_width, target_width, 0],
    };

    let fps = options.fps.max(1);
    let frame_duration = Duration::from_millis(1000 / fps as u64);

    while running.load(Ordering::SeqCst) {
        let start = Instant::now();

        let mut captured = false;
        if let Some((seg, _, shmaddr)) = shm_info {
            if let Ok(cookie) = conn.shm_get_image(
                screen.root,
                0,
                0,
                src_width,
                src_height,
                !0,
                ImageFormat::Z_PIXMAP.into(),
                seg,
                0,
            ) {
                if cookie.reply().is_ok() {
                    let bgra_slice = unsafe {
                        std::slice::from_raw_parts(shmaddr, byte_size)
                    };
                    bgra_to_nv12(
                        bgra_slice,
                        src_width as usize,
                        src_height as usize,
                        target_width as usize,
                        target_height as usize,
                        &mut y_plane,
                        &mut uv_plane,
                    );
                    captured = true;
                }
            }
        }

        if !captured {
            // Fallback to standard xproto get_image
            if let Ok(cookie) = conn.get_image(
                ImageFormat::Z_PIXMAP,
                screen.root,
                0,
                0,
                src_width,
                src_height,
                !0,
            ) {
                if let Ok(reply) = cookie.reply() {
                    bgra_to_nv12(
                        &reply.data,
                        src_width as usize,
                        src_height as usize,
                        target_width as usize,
                        target_height as usize,
                        &mut y_plane,
                        &mut uv_plane,
                    );
                    captured = true;
                }
            }
        }

        if captured {
            frame.data[0] = y_plane.as_ptr() as *const c_void;
            frame.data[1] = uv_plane.as_ptr() as *const c_void;
            if !consumer.sink(&frame) {
                log::warn!("Consumer returned false, stopping X11 screen capture");
                break;
            }
        }

        let elapsed = start.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
    }

    // Cleanup SHM if allocated
    if let Some((seg, shmid, shmaddr)) = shm_info {
        let _ = conn.shm_detach(seg);
        let _ = conn.flush();
        unsafe {
            libc::shmdt(shmaddr as *mut c_void);
            libc::shmctl(shmid, libc::IPC_RMID, null_mut());
        }
    }

    log::info!("X11 screen capture stopped");
    Ok(())
}

/// Convert BGRA pixel buffer into target NV12 planar buffer with fast nearest-neighbor scaling.
pub fn bgra_to_nv12(
    src_bgra: &[u8],
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
        let sy = (dy * y_ratio) >> 16;
        let sy = sy.min(src_h.saturating_sub(1));
        let src_row = sy * src_w * 4;
        let dst_row = dy * dst_w;

        for dx in 0..dst_w {
            let sx = (dx * x_ratio) >> 16;
            let sx = sx.min(src_w.saturating_sub(1));
            let px = src_row + sx * 4;

            if px + 2 < src_bgra.len() {
                let b = src_bgra[px] as i32;
                let g = src_bgra[px + 1] as i32;
                let r = src_bgra[px + 2] as i32;

                // BT.601 Y calculation
                let y = ((19595 * r + 38470 * g + 7471 * b + 32768) >> 16) as u8;
                dst_y[dst_row + dx] = y;

                // Subsample U and V on even dy and dx
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
