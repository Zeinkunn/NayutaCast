use crate::{CaptureHandler, FrameConsumer, Source, SourceType, VideoCaptureSourceDescription};

use std::{
    ffi::c_void,
    fs,
    os::fd::AsRawFd,
    ptr::{null, null_mut},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
};

use common::frame::{VideoFormat, VideoFrame, VideoSubFormat};
use thiserror::Error;

#[derive(Debug, Error)]
#[allow(dead_code)]
pub enum CameraCaptureError {
    #[error("No camera device found")]
    NoCamera,
    #[error("Failed to open camera device: {0}")]
    DeviceOpen(String),
    #[error("Failed to configure camera: {0}")]
    ConfigError(String),
    #[error("Failed to capture frame: {0}")]
    CaptureFailed(String),
    #[error("Thread creation error: {0}")]
    ThreadError(#[from] std::io::Error),
}

// V4L2 ioctl constants
const VIDIOC_QUERYCAP: libc::c_ulong = 0x80685600;
const VIDIOC_S_FMT: libc::c_ulong = 0xc0d05605;
const VIDIOC_REQBUFS: libc::c_ulong = 0xc0145608;
const VIDIOC_QUERYBUF: libc::c_ulong = 0xc0585609;
const VIDIOC_QBUF: libc::c_ulong = 0xc058560f;
const VIDIOC_DQBUF: libc::c_ulong = 0xc0585611;
const VIDIOC_STREAMON: libc::c_ulong = 0x40045612;
const VIDIOC_STREAMOFF: libc::c_ulong = 0x40045613;

const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_MEMORY_MMAP: u32 = 1;
const V4L2_PIX_FMT_YUYV: u32 = 0x56595559; // 'YUYV'
const V4L2_PIX_FMT_NV12: u32 = 0x3231564e; // 'NV12'

#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[repr(C)]
struct V4l2PixFormat {
    width: u32,
    height: u32,
    pixelformat: u32,
    field: u32,
    bytesperline: u32,
    sizeimage: u32,
    colorspace: u32,
    priv_: u32,
    flags: u32,
    ycbcr_enc: u32,
    quantization: u32,
    xfer_func: u32,
}

#[repr(C)]
struct V4l2Format {
    type_: u32,
    fmt: [u8; 200], // Fits V4l2PixFormat
}

#[repr(C)]
struct V4l2RequestBuffers {
    count: u32,
    type_: u32,
    memory: u32,
    reserved: [u32; 2],
}

#[repr(C)]
struct V4l2Timecode {
    type_: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
struct V4l2Buffer {
    index: u32,
    type_: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    timestamp: libc::timeval,
    timecode: V4l2Timecode,
    sequence: u32,
    memory: u32,
    m: usize, // offset in mmap
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

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
        let mut sources = Vec::new();

        if let Ok(entries) = fs::read_dir("/dev") {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();

                if name.starts_with("video") {
                    let path = format!("/dev/{}", name);
                    let display_name = get_device_name(&path)
                        .unwrap_or_else(|| format!("Camera {}", name));

                    let idx = sources.len();
                    sources.push(Source {
                        id: path,
                        name: display_name,
                        index: idx,
                        kind: SourceType::Camera,
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
    let sysfs_name = path.replace("/dev/", "/sys/class/video4linux/");
    let name_path = format!("{}/name", sysfs_name);
    fs::read_to_string(&name_path).ok().map(|s| s.trim().to_string())
}

fn run_v4l2_capture<S: FrameConsumer<Frame = VideoFrame>>(
    running: Arc<AtomicBool>,
    device_path: String,
    width: u32,
    height: u32,
    mut consumer: S,
) -> Result<(), CameraCaptureError> {
    log::info!(
        "V4L2 camera capture starting: {} at {}x{}",
        device_path,
        width,
        height
    );

    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&device_path)
        .map_err(|e| CameraCaptureError::DeviceOpen(format!("{}: {}", device_path, e)))?;

    let fd = file.as_raw_fd();

    unsafe {
        let mut cap: V4l2Capability = std::mem::zeroed();
        if libc::ioctl(fd, VIDIOC_QUERYCAP, &mut cap) < 0 {
            return Err(CameraCaptureError::ConfigError("Failed to query V4L2 caps".into()));
        }

        let mut fmt: V4l2Format = std::mem::zeroed();
        fmt.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        let pix = &mut *(&mut fmt.fmt as *mut _ as *mut V4l2PixFormat);
        pix.width = width;
        pix.height = height;
        pix.pixelformat = V4L2_PIX_FMT_YUYV; // Default to YUYV

        if libc::ioctl(fd, VIDIOC_S_FMT, &mut fmt) < 0 {
            log::warn!("V4L2: Setting YUYV format failed, trying NV12");
            pix.pixelformat = V4L2_PIX_FMT_NV12;
            if libc::ioctl(fd, VIDIOC_S_FMT, &mut fmt) < 0 {
                return Err(CameraCaptureError::ConfigError("Failed to set video format".into()));
            }
        }

        let actual_fmt = pix.pixelformat;
        let actual_w = pix.width;
        let actual_h = pix.height;

        let mut req: V4l2RequestBuffers = std::mem::zeroed();
        req.count = 4;
        req.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        req.memory = V4L2_MEMORY_MMAP;

        if libc::ioctl(fd, VIDIOC_REQBUFS, &mut req) < 0 {
            return Err(CameraCaptureError::ConfigError("Failed to request MMAP buffers".into()));
        }

        let mut buffers = Vec::new();
        for i in 0..req.count {
            let mut buf: V4l2Buffer = std::mem::zeroed();
            buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            buf.memory = V4L2_MEMORY_MMAP;
            buf.index = i;

            if libc::ioctl(fd, VIDIOC_QUERYBUF, &mut buf) < 0 {
                return Err(CameraCaptureError::ConfigError("Failed to query buffer".into()));
            }

            let ptr = libc::mmap(
                null_mut(),
                buf.length as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                fd,
                buf.m as libc::off_t,
            );

            if ptr == libc::MAP_FAILED {
                return Err(CameraCaptureError::ConfigError("mmap buffer failed".into()));
            }

            buffers.push((ptr as *mut u8, buf.length as usize));

            if libc::ioctl(fd, VIDIOC_QBUF, &mut buf) < 0 {
                return Err(CameraCaptureError::ConfigError("Initial QBUF failed".into()));
            }
        }

        let mut type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        if libc::ioctl(fd, VIDIOC_STREAMON, &mut type_) < 0 {
            return Err(CameraCaptureError::ConfigError("VIDIOC_STREAMON failed".into()));
        }

        log::info!("V4L2 stream active: {}x{}, format={:#x}", actual_w, actual_h, actual_fmt);

        let y_size = (actual_w * actual_h) as usize;
        let uv_size = (actual_w * (actual_h / 2)) as usize;
        let mut y_plane = vec![0u8; y_size];
        let mut uv_plane = vec![128u8; uv_size];

        let mut frame = VideoFrame {
            format: VideoFormat::NV12,
            sub_format: VideoSubFormat::SW,
            width: actual_w,
            height: actual_h,
            data: [
                y_plane.as_ptr() as *const c_void,
                uv_plane.as_ptr() as *const c_void,
                null(),
            ],
            linesize: [actual_w, actual_w, 0],
        };

        while running.load(Ordering::SeqCst) {
            let mut pfd = libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            };

            let ret = libc::poll(&mut pfd, 1, 100);
            if ret <= 0 {
                continue;
            }

            let mut buf: V4l2Buffer = std::mem::zeroed();
            buf.type_ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
            buf.memory = V4L2_MEMORY_MMAP;

            if libc::ioctl(fd, VIDIOC_DQBUF, &mut buf) < 0 {
                continue;
            }

            let (ptr, len) = buffers[buf.index as usize];
            let data = std::slice::from_raw_parts(ptr, len.min(buf.bytesused as usize));

            if actual_fmt == V4L2_PIX_FMT_YUYV {
                yuyv_to_nv12(data, actual_w as usize, actual_h as usize, &mut y_plane, &mut uv_plane);
            } else if actual_fmt == V4L2_PIX_FMT_NV12 && data.len() >= y_size + uv_size {
                y_plane.copy_from_slice(&data[..y_size]);
                uv_plane.copy_from_slice(&data[y_size..y_size + uv_size]);
            }

            frame.data[0] = y_plane.as_ptr() as *const c_void;
            frame.data[1] = uv_plane.as_ptr() as *const c_void;

            if !consumer.sink(&frame) {
                log::warn!("Consumer returned false, stopping camera capture");
                let _ = libc::ioctl(fd, VIDIOC_QBUF, &mut buf);
                break;
            }

            let _ = libc::ioctl(fd, VIDIOC_QBUF, &mut buf);
        }

        let _ = libc::ioctl(fd, VIDIOC_STREAMOFF, &mut type_);
        for (ptr, len) in buffers {
            libc::munmap(ptr as *mut c_void, len);
        }
    }

    log::info!("V4L2 camera capture stopped");
    Ok(())
}

fn yuyv_to_nv12(yuyv: &[u8], width: usize, height: usize, y_plane: &mut [u8], uv_plane: &mut [u8]) {
    let mut y_idx = 0;
    let mut uv_idx = 0;

    for row in 0..height {
        let is_even_row = (row & 1) == 0;
        let row_offset = row * width * 2;

        for col in (0..width * 2).step_by(4) {
            let idx = row_offset + col;
            if idx + 3 < yuyv.len() {
                let y0 = yuyv[idx];
                let u = yuyv[idx + 1];
                let y1 = yuyv[idx + 2];
                let v = yuyv[idx + 3];

                if y_idx + 1 < y_plane.len() {
                    y_plane[y_idx] = y0;
                    y_plane[y_idx + 1] = y1;
                    y_idx += 2;
                }

                if is_even_row && uv_idx + 1 < uv_plane.len() {
                    uv_plane[uv_idx] = u;
                    uv_plane[uv_idx + 1] = v;
                    uv_idx += 2;
                }
            }
        }
    }
}
