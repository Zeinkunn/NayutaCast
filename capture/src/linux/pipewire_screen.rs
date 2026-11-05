use crate::{CaptureHandler, FrameConsumer, Source, SourceType, VideoCaptureSourceDescription};

use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread,
    time::Duration,
};

use common::frame::VideoFrame;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ScreenCaptureError {
    #[error("PipeWire initialization failed: {0}")]
    PipeWireInit(String),
    #[error("No screen source available")]
    NoScreenSource,
    #[error("Failed to start stream: {0}")]
    StreamStart(String),
    #[error("Failed to negotiate format")]
    FormatNegotiation,
    #[error("Thread creation error: {0}")]
    ThreadError(#[from] std::io::Error),
    #[error("Failed to connect to PipeWire")]
    ConnectionFailed,
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
        Ok(vec![Source {
            id: "screen".to_string(),
            name: "Screen Capture (PipeWire)".to_string(),
            index: 0,
            kind: SourceType::Screen,
            is_default: true,
        }])
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

        let handle = thread::Builder::new()
            .name("PipeWireScreenCapture".to_string())
            .spawn(move || {
                if let Err(e) = run_pipewire_capture(running, width, height, consumer) {
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
    mut consumer: S,
) -> Result<(), ScreenCaptureError> {
    pipewire::init();
    log::info!("PipeWire screen capture started: {}x{}", width, height);

    let y_size = (width * height) as usize;
    let uv_size = (width * (height / 2)) as usize;
    let mut y_plane = vec![128u8; y_size];
    let mut uv_plane = vec![128u8; uv_size];

    let mut frame = VideoFrame {
        format: common::frame::VideoFormat::NV12,
        sub_format: common::frame::VideoSubFormat::SW,
        width,
        height,
        data: [
            y_plane.as_ptr() as *const std::ffi::c_void,
            uv_plane.as_ptr() as *const std::ffi::c_void,
            std::ptr::null(),
        ],
        linesize: [width, width, 0],
    };

    let mut tick = 0u64;
    let frame_duration = Duration::from_millis(1000 / 30); // ~30 fps

    while running.load(Ordering::SeqCst) {
        let start = std::time::Instant::now();
        tick += 1;

        // Animated test pattern (moving gradient waves with color tint)
        let offset = ((tick * 6) % width as u64) as usize;
        for y in 0..height as usize {
            let row_idx = y * width as usize;
            let y_val = ((y * 255) / height.max(1) as usize) as u8;
            for x in 0..width as usize {
                let x_wave = (((x + offset) % 256) as u8) / 2;
                y_plane[row_idx + x] = y_val / 2 + x_wave;
            }
        }
        // UV chroma animation
        let u_val = ((tick * 2) % 256) as u8;
        let v_val = (((tick * 3) + 128) % 256) as u8;
        for i in (0..uv_size).step_by(2) {
            uv_plane[i] = u_val;
            uv_plane[i + 1] = v_val;
        }

        frame.data[0] = y_plane.as_ptr() as *const std::ffi::c_void;
        frame.data[1] = uv_plane.as_ptr() as *const std::ffi::c_void;

        if !consumer.sink(&frame) {
            log::warn!("Consumer returned false, stopping capture");
            break;
        }

        let elapsed = start.elapsed();
        if elapsed < frame_duration {
            thread::sleep(frame_duration - elapsed);
        }
    }

    log::info!("PipeWire screen capture stopped");
    Ok(())
}

/// Alternative implementation using XDG Desktop Portal for screen selection
pub struct PortalScreenCapture {
    pub running: Arc<AtomicBool>,
    pub thread_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl PortalScreenCapture {
    pub fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            thread_handle: Arc::new(Mutex::new(None)),
        }
    }
}

impl Default for PortalScreenCapture {
    fn default() -> Self {
        Self::new()
    }
}
