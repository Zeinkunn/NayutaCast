//! Linux audio resampling using native libraries
//! 
//! This module provides audio resampling functionality for Linux without FFmpeg dependency.
//! It can use either:
//! - libsamplerate (high quality, CPU-based)
//! - speexdsp (lower quality, faster)
//! - Simple linear interpolation (fastest, lowest quality)

use std::{
    marker::PhantomData,
    sync::{
        Arc,
        atomic::AtomicBool,
        mpsc::{Sender, channel},
    },
    thread,
};

use common::atomic::EasyAtomic;
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub enum AudioSampleFormat {
    I16,
    I32,
    F32,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioSampleDescription {
    pub sample_bits: AudioSampleFormat,
    pub sample_rate: u32,
    pub channels: u8,
}

pub trait AudioResamplerOutput<T>: Send {
    fn output(&mut self, buffer: &[T], frames: u32) -> bool;
}

#[derive(Debug, Error)]
pub enum AudioResamplerError {
    #[error("failed to send buffer to queue")]
    SendBufferError,
    #[error("failed to create resampler")]
    CreateResamplerError,
    #[error("queue is closed")]
    QueueClosed,
    #[error("unsupported format")]
    UnsupportedFormat,
}

pub trait SampleToF64: Copy + Send + 'static {
    fn to_f64(self) -> f64;
}

pub trait SampleFromF32: Copy + Default + Send + 'static {
    fn from_f32(v: f32) -> Self;
}

/// Audio resampler using linear interpolation (simple, no external dependencies)
/// 
/// This is a basic implementation for Linux. For better quality, consider using
/// libsamplerate or speexdsp bindings.
pub struct AudioResampler<I, O> {
    _p: PhantomData<O>,
    tx: Sender<Vec<I>>,
    status: Arc<AtomicBool>,
}

impl<I, O> AudioResampler<I, O>
where
    I: SampleToF64,
    O: SampleFromF32,
{
    pub fn new<T: AudioResamplerOutput<O> + 'static>(
        input: AudioSampleDescription,
        output: AudioSampleDescription,
        mut sink: T,
    ) -> Result<Self, AudioResamplerError> {
        let (tx, rx) = channel::<Vec<I>>();

        let status = Arc::new(AtomicBool::new(true));
        
        let status_ = status.clone();
        let ratio = output.sample_rate as f64 / input.sample_rate as f64;
        let output_channels = output.channels as usize;
        
        thread::spawn(move || {
            while let Ok(buffer) = rx.recv() {
                let input_frames = buffer.len() / input.channels as usize;
                let output_frames = (input_frames as f64 * ratio) as usize;
                let mut output: Vec<O> = vec![O::default(); output_frames * output_channels];
                
                // Simple linear interpolation resampling
                for ch in 0..output_channels {
                    let ch_input = input.channels as usize;
                    
                    for i in 0..output_frames {
                        let src_pos = i as f64 / ratio;
                        let src_idx = src_pos as usize;
                        let frac = src_pos - src_idx as f64;
                        
                        if src_idx + 1 < input_frames {
                            let sample1: f64 = buffer[src_idx * ch_input + ch.min(ch_input - 1)].to_f64();
                            let sample2: f64 = buffer[(src_idx + 1) * ch_input + ch.min(ch_input - 1)].to_f64();
                            let interpolated = sample1 * (1.0 - frac) + sample2 * frac;
                            output[i * output_channels + ch] = O::from_f32(interpolated as f32);
                        } else if src_idx < input_frames {
                            let sample: f64 = buffer[src_idx * ch_input + ch.min(ch_input - 1)].to_f64();
                            output[i * output_channels + ch] = O::from_f32(sample as f32);
                        }
                    }
                }
                
                if !sink.output(&output, output_frames as u32) {
                    break;
                }
            }

            status_.set(false);
        });

        Ok(Self {
            _p: PhantomData::default(),
            status,
            tx,
        })
    }

    pub fn resample<'a>(&'a mut self, buffer: &'a [I]) -> Result<(), AudioResamplerError> {
        if !self.status.get() {
            return Err(AudioResamplerError::QueueClosed);
        }

        self.tx
            .send(buffer.to_vec())
            .map_err(|_| AudioResamplerError::SendBufferError)?;
        Ok(())
    }
}

// Implement conversions for common audio types
impl SampleToF64 for i16 {
    fn to_f64(self) -> f64 {
        self as f64 / 32768.0
    }
}

impl SampleFromF32 for i16 {
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 32767.0) as i16
    }
}

impl SampleToF64 for f32 {
    fn to_f64(self) -> f64 {
        self as f64
    }
}

impl SampleFromF32 for f32 {
    fn from_f32(v: f32) -> Self {
        v
    }
}

impl SampleToF64 for i32 {
    fn to_f64(self) -> f64 {
        self as f64 / 2147483648.0
    }
}

impl SampleFromF32 for i32 {
    fn from_f32(v: f32) -> Self {
        (v.clamp(-1.0, 1.0) * 2147483647.0) as i32
    }
}
