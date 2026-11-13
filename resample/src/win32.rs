//! Hardware-accelerated Direct3D 11 video format conversion and scaling.

use std::mem::ManuallyDrop;

use common::{
    Size,
    frame::VideoFormat,
    win32::{
        Direct3DDevice,
        windows::{
            Win32::Graphics::{
                Direct3D11::{
                    D3D11_BIND_RENDER_TARGET, D3D11_CPU_ACCESS_READ, D3D11_MAP_READ,
                    D3D11_MAPPED_SUBRESOURCE, D3D11_RESOURCE_MISC_SHARED, D3D11_TEX2D_VPIV,
                    D3D11_TEX2D_VPOV, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
                    D3D11_USAGE_STAGING, D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                    D3D11_VIDEO_PROCESSOR_CONTENT_DESC, D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC,
                    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0,
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC,
                    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0, D3D11_VIDEO_PROCESSOR_STREAM,
                    D3D11_VIDEO_USAGE_PLAYBACK_NORMAL, D3D11_VPIV_DIMENSION_TEXTURE2D,
                    D3D11_VPOV_DIMENSION_TEXTURE2D, ID3D11Texture2D, ID3D11VideoContext,
                    ID3D11VideoDevice, ID3D11VideoProcessor, ID3D11VideoProcessorEnumerator,
                    ID3D11VideoProcessorInputView, ID3D11VideoProcessorOutputView,
                },
                Dxgi::Common::{
                    DXGI_FORMAT, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12,
                    DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
                },
            },
            core::{Error, HRESULT, Interface, Result},
        },
    },
};

const E_FAIL: HRESULT = HRESULT(0x80004005_u32 as i32);
const E_INVALIDARG: HRESULT = HRESULT(0x80070057_u32 as i32);

/// Surface specification for D3D11 video resampling.
#[derive(Debug, Clone, Copy)]
pub enum Resource {
    /// Pixel format and frame dimensions of a D3D11 texture resource.
    Default(VideoFormat, Size),
}

impl Resource {
    fn format_and_size(self) -> (VideoFormat, Size) {
        match self {
            Self::Default(format, size) => (format, size),
        }
    }
}

/// Configuration for creating a [`VideoResampler`].
pub struct VideoResamplerOptions {
    /// Direct3D 11 device and immediate context used for processing.
    pub direct3d: Direct3DDevice,
    /// Input surface format and dimensions.
    pub input: Resource,
    /// Output surface format and dimensions.
    pub output: Resource,
}

/// Tightly packed CPU-readable frame buffer copied from a D3D11 texture.
pub struct VideoBuffer<'a> {
    buffer: &'a [u8],
    stride: u32,
}

impl VideoBuffer<'_> {
    /// Returns a pointer to the start of the packed pixel buffer.
    pub fn buffer(&self) -> *const u8 {
        self.buffer.as_ptr()
    }

    /// Returns the row stride in bytes of the packed pixel buffer.
    pub fn stride(&self) -> u32 {
        self.stride
    }
}

/// GPU video processor for converting and scaling Direct3D 11 textures.
pub struct VideoResampler {
    direct3d: Direct3DDevice,
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    enumerator: ID3D11VideoProcessorEnumerator,
    processor: ID3D11VideoProcessor,
    output_texture: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    output_format: VideoFormat,
    output_size: Size,
    staging_texture: Option<ID3D11Texture2D>,
    staging_buffer: Vec<u8>,
}

impl VideoResampler {
    /// Initializes a Direct3D 11 video processor and output surface.
    pub fn new(options: VideoResamplerOptions) -> Result<Self> {
        let (input_format, input_size) = options.input.format_and_size();
        let (output_format, output_size) = options.output.format_and_size();

        let _ = dxgi_format(input_format)?;
        let out_dxgi = dxgi_format(output_format)?;

        let video_device: ID3D11VideoDevice = options.direct3d.device.cast()?;
        let video_context: ID3D11VideoContext = options.direct3d.context.cast()?;

        let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            InputWidth: input_size.width,
            InputHeight: input_size.height,
            OutputFrameRate: DXGI_RATIONAL {
                Numerator: 60,
                Denominator: 1,
            },
            OutputWidth: output_size.width,
            OutputHeight: output_size.height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let enumerator = unsafe { video_device.CreateVideoProcessorEnumerator(&content_desc)? };
        let processor = unsafe { video_device.CreateVideoProcessor(&enumerator, 0)? };

        let output_desc = D3D11_TEXTURE2D_DESC {
            Width: output_size.width,
            Height: output_size.height,
            MipLevels: 1,
            ArraySize: 1,
            Format: out_dxgi,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_DEFAULT,
            BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
            CPUAccessFlags: 0,
            MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
        };

        let mut output_texture = None;
        unsafe {
            options.direct3d.device.CreateTexture2D(
                &output_desc,
                None,
                Some(&mut output_texture),
            )?;
        }
        let output_texture = output_texture.ok_or_else(|| Error::from(E_FAIL))?;

        let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
            ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
            },
        };

        let mut output_view = None;
        unsafe {
            video_device.CreateVideoProcessorOutputView(
                &output_texture,
                &enumerator,
                &output_view_desc,
                Some(&mut output_view),
            )?;
        }
        let output_view = output_view.ok_or_else(|| Error::from(E_FAIL))?;

        Ok(Self {
            direct3d: options.direct3d,
            video_device,
            video_context,
            enumerator,
            processor,
            output_texture,
            output_view,
            output_format,
            output_size,
            staging_texture: None,
            staging_buffer: Vec::new(),
        })
    }

    /// Creates a video processor input view wrapping an input texture slice.
    pub fn create_input_view(
        &self,
        texture: &ID3D11Texture2D,
        index: u32,
    ) -> Result<ID3D11VideoProcessorInputView> {
        let desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV {
                    MipSlice: 0,
                    ArraySlice: index,
                },
            },
        };

        let mut input_view = None;
        unsafe {
            self.video_device.CreateVideoProcessorInputView(
                texture,
                &self.enumerator,
                &desc,
                Some(&mut input_view),
            )?;
        }

        input_view.ok_or_else(|| Error::from(E_FAIL))
    }

    /// Executes color conversion and scaling onto the internal output surface.
    pub fn process(&mut self, view: Option<ID3D11VideoProcessorInputView>) -> Result<()> {
        if view.is_none() {
            return Ok(());
        }

        let mut streams = [D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            OutputIndex: 0,
            InputFrameOrField: 0,
            PastFrames: 0,
            FutureFrames: 0,
            ppPastSurfaces: std::ptr::null_mut(),
            pInputSurface: ManuallyDrop::new(view),
            ppFutureSurfaces: std::ptr::null_mut(),
            ppPastSurfacesRight: std::ptr::null_mut(),
            pInputSurfaceRight: ManuallyDrop::new(None),
            ppFutureSurfacesRight: std::ptr::null_mut(),
        }];

        let result = unsafe {
            self.video_context
                .VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams)
        };

        // Release the temporary input view COM ref held in ManuallyDrop.
        unsafe {
            ManuallyDrop::drop(&mut streams[0].pInputSurface);
        }

        result
    }

    /// Returns a reference to the GPU output texture.
    pub fn get_output(&self) -> &ID3D11Texture2D {
        &self.output_texture
    }

    /// Copies the GPU output texture to staging memory and packs scanlines.
    pub fn get_output_buffer(&mut self) -> Result<VideoBuffer<'_>> {
        let staging = match &self.staging_texture {
            Some(tex) => tex.clone(),
            None => {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.output_size.width,
                    Height: self.output_size.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: dxgi_format(self.output_format)?,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };

                let mut tex = None;
                unsafe {
                    self.direct3d
                        .device
                        .CreateTexture2D(&desc, None, Some(&mut tex))?;
                }
                let tex = tex.ok_or_else(|| Error::from(E_FAIL))?;
                self.staging_texture = Some(tex.clone());
                tex
            }
        };

        unsafe {
            self.direct3d
                .context
                .CopyResource(&staging, &self.output_texture);
        }

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        unsafe {
            self.direct3d
                .context
                .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        }

        let height = self.output_size.height as usize;
        let pitch = mapped.RowPitch as usize;
        let stride = self.output_stride();
        let row_bytes = stride as usize;
        let rows = match self.output_format {
            VideoFormat::NV12 => height + height / 2,
            VideoFormat::BGRA | VideoFormat::RGBA => height,
            VideoFormat::I420 => {
                unsafe { self.direct3d.context.Unmap(&staging, 0) };
                return Err(Error::from(E_INVALIDARG));
            }
        };

        let src = mapped.pData as *const u8;
        if src.is_null() || pitch < row_bytes {
            unsafe { self.direct3d.context.Unmap(&staging, 0) };
            return Err(Error::from(E_FAIL));
        }

        self.staging_buffer.resize(row_bytes * rows, 0);
        for row in 0..rows {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    src.add(row * pitch),
                    self.staging_buffer.as_mut_ptr().add(row * row_bytes),
                    row_bytes,
                );
            }
        }

        unsafe {
            self.direct3d.context.Unmap(&staging, 0);
        }

        Ok(VideoBuffer {
            buffer: &self.staging_buffer,
            stride,
        })
    }

    fn output_stride(&self) -> u32 {
        match self.output_format {
            VideoFormat::NV12 | VideoFormat::I420 => self.output_size.width,
            VideoFormat::BGRA | VideoFormat::RGBA => self.output_size.width * 4,
        }
    }
}

fn dxgi_format(format: VideoFormat) -> Result<DXGI_FORMAT> {
    match format {
        VideoFormat::NV12 => Ok(DXGI_FORMAT_NV12),
        VideoFormat::BGRA => Ok(DXGI_FORMAT_B8G8R8A8_UNORM),
        VideoFormat::RGBA => Ok(DXGI_FORMAT_R8G8B8A8_UNORM),
        VideoFormat::I420 => Err(Error::from(E_INVALIDARG)),
    }
}
