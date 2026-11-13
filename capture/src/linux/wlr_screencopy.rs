use std::ffi::c_void;
use std::fs::File;
use std::os::fd::AsFd;
use std::ptr::null;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use common::frame::{VideoFormat, VideoFrame, VideoSubFormat};
use memmap2::MmapMut;
use nix::sys::memfd::{MemFdCreateFlag, memfd_create};
use nix::unistd::ftruncate;
use wayland_client::protocol::{wl_buffer, wl_output, wl_registry, wl_shm, wl_shm_pool};
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum, delegate_noop};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1, zwlr_screencopy_manager_v1,
};

use crate::FrameConsumer;
use super::pipewire_screen::{PixelOrder, ScreenCaptureError, packed_4byte_to_nv12};

struct OutputEntry {
    wl_output: wl_output::WlOutput,
    name: Option<String>,
}

struct ShmBacking {
    width: u32,
    height: u32,
    stride: u32,
    format: wl_shm::Format,
    _file: File,
    mmap: MmapMut,
    pool: wl_shm_pool::WlShmPool,
    buffer: wl_buffer::WlBuffer,
}

impl ShmBacking {
    fn new(
        shm: &wl_shm::WlShm,
        qh: &QueueHandle<WlrState>,
        width: u32,
        height: u32,
        stride: u32,
        format: wl_shm::Format,
    ) -> Result<Self, ScreenCaptureError> {
        let size = (stride as usize) * (height as usize);
        let fd = memfd_create(c"nayutacast-wlr-shm", MemFdCreateFlag::MFD_CLOEXEC)
            .map_err(|e| ScreenCaptureError::StreamStart(format!("memfd_create: {}", e)))?;
        ftruncate(&fd, size as i64)
            .map_err(|e| ScreenCaptureError::StreamStart(format!("ftruncate: {}", e)))?;

        let file = File::from(fd);
        let mmap = unsafe {
            MmapMut::map_mut(&file)
                .map_err(|e| ScreenCaptureError::StreamStart(format!("mmap: {}", e)))?
        };

        let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            stride as i32,
            format,
            qh,
            (),
        );

        Ok(Self {
            width,
            height,
            stride,
            format,
            _file: file,
            mmap,
            pool,
            buffer,
        })
    }

    fn matches(&self, width: u32, height: u32, stride: u32, format: wl_shm::Format) -> bool {
        self.width == width
            && self.height == height
            && self.stride == stride
            && self.format == format
    }
}

impl Drop for ShmBacking {
    fn drop(&mut self) {
        self.buffer.destroy();
        self.pool.destroy();
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FramePhase {
    Pending,
    Ready,
    Failed,
}

struct PendingBufferSpec {
    format: wl_shm::Format,
    width: u32,
    height: u32,
    stride: u32,
}

struct WlrState {
    shm: Option<wl_shm::WlShm>,
    manager: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    manager_version: u32,
    outputs: Vec<OutputEntry>,
    backing: Option<ShmBacking>,
    pending_specs: Vec<PendingBufferSpec>,
    y_invert: bool,
    copy_sent: bool,
    phase: FramePhase,
}

impl WlrState {
    fn select_best_spec(&self) -> Option<&PendingBufferSpec> {
        // Prefer XRGB8888 / ARGB8888 (little-endian BGRx) first, then XBGR8888 / ABGR8888 (RGBx)
        let preferred_order = [
            wl_shm::Format::Xrgb8888,
            wl_shm::Format::Argb8888,
            wl_shm::Format::Xbgr8888,
            wl_shm::Format::Abgr8888,
        ];
        for fmt in preferred_order {
            if let Some(s) = self.pending_specs.iter().find(|s| s.format == fmt) {
                return Some(s);
            }
        }
        None
    }

    fn trigger_copy(
        &mut self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        qh: &QueueHandle<Self>,
    ) {
        if self.copy_sent {
            return;
        }
        let Some(spec) = self.select_best_spec() else {
            return;
        };
        let (width, height, stride, format) =
            (spec.width, spec.height, spec.stride, spec.format);
        let Some(shm) = &self.shm else {
            self.phase = FramePhase::Failed;
            return;
        };

        let need_new = self
            .backing
            .as_ref()
            .map(|b| !b.matches(width, height, stride, format))
            .unwrap_or(true);

        if need_new {
            self.backing = None;
            match ShmBacking::new(shm, qh, width, height, stride, format) {
                Ok(b) => self.backing = Some(b),
                Err(e) => {
                    log::error!("wlr-screencopy shm alloc failed: {:?}", e);
                    self.phase = FramePhase::Failed;
                    return;
                }
            }
        }

        if let Some(backing) = &self.backing {
            frame.copy(&backing.buffer);
            self.copy_sent = true;
        }
    }
}

delegate_noop!(WlrState: ignore wl_shm::WlShm);
delegate_noop!(WlrState: ignore wl_shm_pool::WlShmPool);
delegate_noop!(WlrState: ignore wl_buffer::WlBuffer);
delegate_noop!(WlrState: ignore zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1);

impl Dispatch<wl_registry::WlRegistry, ()> for WlrState {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        {
            match interface.as_str() {
                "wl_shm" => {
                    state.shm = Some(registry.bind::<wl_shm::WlShm, _, _>(name, 1, qh, ()));
                }
                "zwlr_screencopy_manager_v1" => {
                    let v = version.min(3);
                    state.manager_version = v;
                    state.manager = Some(
                        registry.bind::<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1, _, _>(
                            name, v, qh, (),
                        ),
                    );
                }
                "wl_output" => {
                    let v = version.min(4);
                    let idx = state.outputs.len();
                    let wl_out = registry.bind::<wl_output::WlOutput, _, _>(name, v, qh, idx);
                    state.outputs.push(OutputEntry {
                        wl_output: wl_out,
                        name: None,
                    });
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<wl_output::WlOutput, usize> for WlrState {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        idx: &usize,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_output::Event::Name { name } = event {
            if let Some(entry) = state.outputs.get_mut(*idx) {
                entry.name = Some(name);
            }
        }
    }
}

impl Dispatch<zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1, ()> for WlrState {
    fn event(
        state: &mut Self,
        frame: &zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_screencopy_frame_v1::Event::Buffer {
                format,
                width,
                height,
                stride,
            } => {
                if let WEnum::Value(fmt) = format {
                    state.pending_specs.push(PendingBufferSpec {
                        format: fmt,
                        width,
                        height,
                        stride,
                    });
                    if state.manager_version < 3 {
                        state.trigger_copy(frame, qh);
                    }
                }
            }
            zwlr_screencopy_frame_v1::Event::BufferDone => {
                state.trigger_copy(frame, qh);
            }
            zwlr_screencopy_frame_v1::Event::Flags { flags } => {
                if let WEnum::Value(f) = flags {
                    state.y_invert = f.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
                }
            }
            zwlr_screencopy_frame_v1::Event::Ready { .. } => {
                state.phase = FramePhase::Ready;
            }
            zwlr_screencopy_frame_v1::Event::Failed => {
                state.phase = FramePhase::Failed;
            }
            _ => {}
        }
    }
}

pub fn run_wlr_screencopy<S: FrameConsumer<Frame = VideoFrame>>(
    running: &AtomicBool,
    dst_width: u32,
    dst_height: u32,
    fps: u8,
    monitor_output: Option<&str>,
    consumer: &mut S,
) -> Result<(), ScreenCaptureError> {
    let conn = Connection::connect_to_env()
        .map_err(|e| ScreenCaptureError::StreamStart(format!("wayland connect: {}", e)))?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = WlrState {
        shm: None,
        manager: None,
        manager_version: 1,
        outputs: Vec::new(),
        backing: None,
        pending_specs: Vec::new(),
        y_invert: false,
        copy_sent: false,
        phase: FramePhase::Pending,
    };

    let _registry = display.get_registry(&qh, ());
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| ScreenCaptureError::StreamStart(format!("wayland roundtrip 1: {}", e)))?;
    // Second roundtrip collects wl_output::Event::Name
    event_queue
        .roundtrip(&mut state)
        .map_err(|e| ScreenCaptureError::StreamStart(format!("wayland roundtrip 2: {}", e)))?;

    if state.manager.is_none() || state.shm.is_none() {
        return Err(ScreenCaptureError::StreamStart(
            "zwlr_screencopy_manager_v1 or wl_shm unavailable".into(),
        ));
    }

    if let Some(target_name) = monitor_output {
        // Give newly created HEADLESS-* outputs up to 400ms to announce if needed
        let deadline = Instant::now() + Duration::from_millis(400);
        while !state
            .outputs
            .iter()
            .any(|o| o.name.as_deref() == Some(target_name))
            && Instant::now() < deadline
        {
            let _ = event_queue.roundtrip(&mut state);
            thread::sleep(Duration::from_millis(30));
        }
    }

    let selected_idx = if let Some(target) = monitor_output {
        state
            .outputs
            .iter()
            .position(|o| o.name.as_deref() == Some(target))
            .unwrap_or(0)
    } else {
        0
    };

    let Some(output) = state.outputs.get(selected_idx).map(|o| o.wl_output.clone()) else {
        return Err(ScreenCaptureError::NoScreenSource);
    };
    let output_label = state.outputs[selected_idx]
        .name
        .clone()
        .unwrap_or_else(|| "default".to_string());
    log::info!(
        "wlr-screencopy capturing output '{}' -> {}x{} @ {}fps",
        output_label,
        dst_width,
        dst_height,
        fps
    );

    let manager = state.manager.clone().unwrap();
    let y_size = (dst_width * dst_height) as usize;
    let uv_size = (dst_width * (dst_height / 2)) as usize;
    let mut y_plane = vec![16u8; y_size];
    let mut uv_plane = vec![128u8; uv_size];

    let mut frame_out = VideoFrame {
        format: VideoFormat::NV12,
        sub_format: VideoSubFormat::SW,
        width: dst_width,
        height: dst_height,
        data: [
            y_plane.as_ptr() as *const c_void,
            uv_plane.as_ptr() as *const c_void,
            null(),
        ],
        linesize: [dst_width, dst_width, 0],
    };

    let frame_duration = Duration::from_micros(1_000_000 / (fps.max(1) as u64));

    while running.load(Ordering::SeqCst) {
        let tick_start = Instant::now();
        state.pending_specs.clear();
        state.y_invert = false;
        state.copy_sent = false;
        state.phase = FramePhase::Pending;

        let sc_frame = manager.capture_output(1, &output, &qh, ());
        while state.phase == FramePhase::Pending && running.load(Ordering::SeqCst) {
            if let Err(e) = event_queue.blocking_dispatch(&mut state) {
                sc_frame.destroy();
                return Err(ScreenCaptureError::StreamStart(format!(
                    "wayland dispatch: {}",
                    e
                )));
            }
        }

        if state.phase == FramePhase::Ready
            && let Some(backing) = &state.backing
        {
            let order = match backing.format {
                wl_shm::Format::Xbgr8888 | wl_shm::Format::Abgr8888 => PixelOrder::Rgbx,
                _ => PixelOrder::Bgrx,
            };
            packed_4byte_to_nv12(
                &backing.mmap,
                backing.width as usize,
                backing.height as usize,
                backing.stride as usize,
                order,
                state.y_invert,
                dst_width as usize,
                dst_height as usize,
                &mut y_plane,
                &mut uv_plane,
            );
            frame_out.data[0] = y_plane.as_ptr() as *const c_void;
            frame_out.data[1] = uv_plane.as_ptr() as *const c_void;

            sc_frame.destroy();

            if !consumer.sink(&frame_out) {
                break;
            }
        } else {
            sc_frame.destroy();
            if state.phase == FramePhase::Failed {
                return Err(ScreenCaptureError::StreamStart(
                    "zwlr_screencopy_frame failed".into(),
                ));
            }
        }

        let elapsed = tick_start.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
    }

    Ok(())
}
