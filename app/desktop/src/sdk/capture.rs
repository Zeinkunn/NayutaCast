use common::frame::{VideoFormat, VideoFrame};
use egui::ColorImage;
use parking_lot::Mutex;
use std::sync::Arc;

/// Live frame sink for updating egui texture
#[derive(Clone)]
pub struct LiveFrameSink {
    pub frame_buffer: Arc<Mutex<Option<ColorImage>>>,
}

impl LiveFrameSink {
    pub fn new() -> (Self, Arc<Mutex<Option<ColorImage>>>) {
        let buffer = Arc::new(Mutex::new(None));
        (
            Self {
                frame_buffer: buffer.clone(),
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

        let mut rgba = vec![255u8; width * height * 4];

        match frame.format {
            VideoFormat::RGBA => {
                let line_bytes = frame.linesize[0] as usize;
                let y_ptr = frame.data[0] as *const u8;
                for y in 0..height {
                    let src_row =
                        unsafe { std::slice::from_raw_parts(y_ptr.add(y * line_bytes), width * 4) };
                    let dst_offset = y * width * 4;
                    rgba[dst_offset..dst_offset + width * 4].copy_from_slice(src_row);
                }
            }
            VideoFormat::BGRA => {
                let line_bytes = frame.linesize[0] as usize;
                let y_ptr = frame.data[0] as *const u8;
                for y in 0..height {
                    let src_row =
                        unsafe { std::slice::from_raw_parts(y_ptr.add(y * line_bytes), width * 4) };
                    let dst_offset = y * width * 4;
                    for x in 0..width {
                        let s_idx = x * 4;
                        let d_idx = dst_offset + x * 4;
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
                    for y in 0..height {
                        let y_row =
                            unsafe { std::slice::from_raw_parts(y_ptr.add(y * y_line), width) };
                        let uv_row = unsafe {
                            std::slice::from_raw_parts(uv_ptr.add((y / 2) * uv_line), width)
                        };

                        for (x, &y_byte) in y_row.iter().enumerate() {
                            let y_val = y_byte as f32;
                            let uv_x = x & !1;
                            let u_val = uv_row[uv_x] as f32 - 128.0;
                            let v_val = uv_row[uv_x + 1] as f32 - 128.0;

                            let r = (y_val + 1.402 * v_val).clamp(0.0, 255.0) as u8;
                            let g = (y_val - 0.344136 * u_val - 0.714136 * v_val).clamp(0.0, 255.0)
                                as u8;
                            let b = (y_val + 1.772 * u_val).clamp(0.0, 255.0) as u8;

                            let d_idx = (y * width + x) * 4;
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

        let color_image = ColorImage::from_rgba_unmultiplied([width, height], &rgba);
        *self.frame_buffer.lock() = Some(color_image);
        true
    }
}
