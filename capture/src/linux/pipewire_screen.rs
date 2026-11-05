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
    _width: u32,
    _height: u32,
    mut _consumer: S,
) -> Result<(), ScreenCaptureError> {
    pipewire::init();
    log::info!("PipeWire screen capture started");

    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(16));
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
