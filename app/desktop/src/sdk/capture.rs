use std::sync::Arc;
use std::time::{Duration, Instant};

use common::frame::{VideoFormat, VideoFrame};
use egui::ColorImage;
use parking_lot::Mutex;

const PREVIEW_INTERVAL: Duration = Duration::from_millis(80);
const PREVIEW_MAX_WIDTH: usize = 480;

/// Live frame sink for updating egui texture.
/// Downsamples and throttles preview conversion so it never stalls capture.
#[derive(Clone)]
pub struct LiveFrameSink {
    pub frame_buffer: Arc<Mutex<Option<ColorImage>>>,
    last_preview: Arc<Mutex<Option<Instant>>>,
}

impl LiveFrameSink {
    pub fn new() -> (Self, Arc<Mutex<Option<ColorImage>>>) {
        let buffer = Arc::new(Mutex::new(None));
        (
            Self {
                frame_buffer: buffer.clone(),
                last_preview: Arc::new(Mutex::new(None)),
            },
            buffer,
        )
    }
}

impl nayutacast::MediaStreamSink for LiveFrameSink {
    fn video(&self, frame: &VideoFrame) -> bool {
        let width = frame.width as usize;
        let height = frame.height as usize;
        if width == 0 || height == 0 || frame.data[0].is_null() {
            return true;
        }

        let now = Instant::now();
        {
            let mut last = self.last_preview.lock();
            if let Some(prev) = *last
                && now.duration_since(prev) < PREVIEW_INTERVAL
            {
                return true;
            }
            *last = Some(now);
        }

        let step = (width / PREVIEW_MAX_WIDTH).max(1);
        let out_w = width.div_ceil(step);
        let out_h = height.div_ceil(step);
        let mut rgba = vec![255u8; out_w * out_h * 4];

        match frame.format {
            VideoFormat::RGBA => {
                let line_bytes = frame.linesize[0] as usize;
                let y_ptr = frame.data[0] as *const u8;
                for (oy, y) in (0..height).step_by(step).enumerate() {
                    let src_row =
                        unsafe { std::slice::from_raw_parts(y_ptr.add(y * line_bytes), width * 4) };
                    let dst_row_off = oy * out_w * 4;
                    for (ox, x) in (0..width).step_by(step).enumerate() {
                        let s_idx = x * 4;
                        let d_idx = dst_row_off + ox * 4;
                        rgba[d_idx..d_idx + 4].copy_from_slice(&src_row[s_idx..s_idx + 4]);
                    }
                }
            }
            VideoFormat::BGRA => {
                let line_bytes = frame.linesize[0] as usize;
                let y_ptr = frame.data[0] as *const u8;
                for (oy, y) in (0..height).step_by(step).enumerate() {
                    let src_row =
                        unsafe { std::slice::from_raw_parts(y_ptr.add(y * line_bytes), width * 4) };
                    let dst_row_off = oy * out_w * 4;
                    for (ox, x) in (0..width).step_by(step).enumerate() {
                        let s_idx = x * 4;
                        let d_idx = dst_row_off + ox * 4;
                        rgba[d_idx] = src_row[s_idx + 2]; // R
                        rgba[d_idx + 1] = src_row[s_idx + 1]; // G
                        rgba[d_idx + 2] = src_row[s_idx]; // B
                        rgba[d_idx + 3] = src_row[s_idx + 3]; // A
                    }
                }
            }
            VideoFormat::NV12 => {
                let y_line = frame.linesize[0] as usize;
                let uv_line = frame.linesize[1] as usize;
                let y_ptr = frame.data[0] as *const u8;
                let uv_ptr = frame.data[1] as *const u8;

                if !uv_ptr.is_null() {
                    for (oy, y) in (0..height).step_by(step).enumerate() {
                        let y_row =
                            unsafe { std::slice::from_raw_parts(y_ptr.add(y * y_line), width) };
                        let uv_row = unsafe {
                            std::slice::from_raw_parts(uv_ptr.add((y / 2) * uv_line), width)
                        };
                        let dst_row_off = oy * out_w * 4;

                        for (ox, x) in (0..width).step_by(step).enumerate() {
                            let y_val = y_row[x] as i32;
                            let uv_x = x & !1;
                            let u_val = uv_row[uv_x] as i32 - 128;
                            let v_val = uv_row[uv_x + 1] as i32 - 128;

                            let r = (y_val + ((1436 * v_val) >> 10)).clamp(0, 255) as u8;
                            let g =
                                (y_val - ((352 * u_val + 731 * v_val) >> 10)).clamp(0, 255) as u8;
                            let b = (y_val + ((1815 * u_val) >> 10)).clamp(0, 255) as u8;

                            let d_idx = dst_row_off + ox * 4;
                            rgba[d_idx] = r;
                            rgba[d_idx + 1] = g;
                            rgba[d_idx + 2] = b;
                            rgba[d_idx + 3] = 255;
                        }
                    }
                }
            }
            _ => {
                // Other formats
            }
        }

        let color_image = ColorImage::from_rgba_unmultiplied([out_w, out_h], &rgba);
        *self.frame_buffer.lock() = Some(color_image);
        true
    }
}
