use crate::{CaptureHandler, FrameConsumer, Source, SourceType, VideoCaptureSourceDescription};

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
    thread,
    time::Duration,
};

use common::{
    Size,
    atomic::EasyAtomic,
    frame::{VideoFormat, VideoFrame, VideoSubFormat},
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CameraCaptureError {
    #[error("No camera device found")]
    NoCamera,
    #[error("Failed to open camera device: {0}")]
    DeviceOpen(String),
    #[error("Failed to configure camera: {0}")]
    ConfigError(String),
    #[error("Failed to capture frame: {0}")]
    CaptureFailed(String),
    #[error("Unsupported format")]
    UnsupportedFormat,
    #[error("Thread creation error: {0}")]
    ThreadError(#[from] std::io::Error),
}

/// V4L2 camera capture implementation
pub struct V4L2CameraCapture {
    running: Arc<AtomicBool>,
    thread_handle: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
}

impl Default for V4L2CameraCapture {
    fn default() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(false)),
            thread_handle: Arc::new(Mutex::new(None)),
        }
    }
}

impl CaptureHandler for V4L2CameraCapture {
    type Frame = VideoFrame;
    type Error = CameraCaptureError;
    type CaptureOptions = VideoCaptureSourceDescription;

    fn get_sources() -> Result<Vec<Source>, Self::Error> {
        // Enumerate V4L2 devices from /dev/video*
        let mut sources = Vec::new();
        
        if let Ok(entries) = fs::read_dir("/dev") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                
                if name.starts_with("video") {
                    let path = format!("/dev/{}", name);
                    
                    // Try to get device info
                    let display_name = get_device_name(&path)
                        .unwrap_or_else(|| format!("Camera {}", name));
                    
                    let idx = sources.len();
                    sources.push(Source {
                        id: path,
                        name: display_name,
                        index: idx,
                        kind: SourceType::Screen,
                        is_default: idx == 0,
                    });
                }
            }
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

        let device_path = options.source.id.clone();
        let width = options.size.width;
        let height = options.size.height;

        let handle = thread::Builder::new()
            .name("V4L2CameraCapture".to_string())
            .spawn(move || {
                if let Err(e) = run_v4l2_capture(running, device_path, width, height, consumer) {
                    log::error!("V4L2 camera capture error: {:?}", e);
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

fn get_device_name(path: &str) -> Option<String> {
    // Try to read device name from sysfs
    // e.g., /sys/class/video4linux/video0/name
    let sysfs_name = path.replace("/dev/", "/sys/class/video4linux/");
    let name_path = format!("{}/name", sysfs_name);
    
    fs::read_to_string(&name_path)
        .ok()
        .map(|s| s.trim().to_string())
}

fn run_v4l2_capture<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    device_path: String,
    width: u32,
    height: u32,
    mut consumer: S,
) -> Result<(), CameraCaptureError> {
    log::info!(
        "V4L2 camera capture started: {} at {}x{}",
        device_path,
        width,
        height
    );

    // V4L2 implementation would:
    // 1. Open device with nix::fcntl::open()
    // 2. Query capabilities with VIDIOC_QUERYCAP
    // 3. Set format with VIDIOC_S_FMT (prefer MJPEG for USB cameras)
    // 4. Request buffers with VIDIOC_REQBUFS (MMAP mode)
    // 5. Map buffers with mmap
    // 6. Start streaming with VIDIOC_STREAMON
    // 7. Dequeue/queue buffers in a loop
    
    // Placeholder implementation
    while running.load(Ordering::SeqCst) {
        thread::sleep(Duration::from_millis(33)); // ~30fps
    }

    log::info!("V4L2 camera capture stopped");
    Ok(())
}

/// Camera format information
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CameraFormat {
    Mjpeg,
    Yuyv,
    Nv12,
    Bgr24,
    Rgb24,
}

impl CameraFormat {
    pub fn to_video_format(&self) -> VideoFormat {
        match self {
            Self::Mjpeg => VideoFormat::NV12, // Would decode MJPEG
            Self::Yuyv => VideoFormat::NV12,  // Would convert YUYV
            Self::Nv12 => VideoFormat::NV12,
            Self::Bgr24 => VideoFormat::BGRA,
            Self::Rgb24 => VideoFormat::RGBA,
        }
    }
}
