//! Audio resampling module
//!
//! On Linux: Uses native linear interpolation (no FFmpeg dependency)
//! On Windows/macOS: Uses FFmpeg swresample

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub use linux::{
    AudioResampler, AudioResamplerError, AudioResamplerOutput, AudioSampleDescription,
    AudioSampleFormat, SampleFromF32, SampleToF64,
};

#[cfg(any(target_os = "windows", target_os = "macos"))]
mod ffmpeg;

#[cfg(any(target_os = "windows", target_os = "macos"))]
pub use ffmpeg::{
    AudioResampler, AudioResamplerError, AudioResamplerOutput, AudioSampleDescription,
    AudioSampleFormat,
};
