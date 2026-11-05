//! FFmpeg-based audio resampling for Windows and macOS

use std::{
    marker::PhantomData,
    ptr::null_mut,
    sync::{
        Arc,
        atomic::AtomicBool,
        mpsc::{Sender, channel},
    },
    thread,
};

use common::atomic::EasyAtomic;
use ffmpeg::*;
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub enum AudioSampleFormat {
    I16,
    I32,
    F32,
}

impl Into<AVSampleFormat> for AudioSampleFormat {
    fn into(self) -> AVSampleFormat {
        match self {
            Self::I16 => AVSampleFormat::AV_SAMPLE_FMT_S16,
            Self::I32 => AVSampleFormat::AV_SAMPLE_FMT_S32,
            Self::F32 => AVSampleFormat::AV_SAMPLE_FMT_FLT,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AudioSampleDescription {
    pub sample_bits: AudioSampleFormat,
    pub sample_rate: u32,
    pub channels: u8,
}

impl AudioSampleDescription {
    fn channel_layout(&self) -> AVChannelLayout {
        AVChannelLayout {
            order: AVChannelOrder::AV_CHANNEL_ORDER_NATIVE,
            nb_channels: self.channels as i32,
            u: AVChannelLayout__bindgen_ty_1 {
                mask: match self.channels {
                    1 => AV_CH_LAYOUT_MONO,
                    2 => AV_CH_LAYOUT_STEREO,
                    _ => unimplemented!("unsupported audio channels={}", self.channels),
                },
            },
            opaque: null_mut(),
        }
    }
}

pub trait AudioResamplerOutput<T>: Send {
    fn output(&mut self, buffer: &[T], frames: u32) -> bool;
}

#[derive(Debug, Error)]
pub enum AudioResamplerError {
    #[error("failed to send buffer to queue")]
    SendBufferError,
    #[error("failed to create swresample")]
    CreateSwresampleError,
    #[error("queue is closed")]
    QueueClosed,
}

/// Audio resampler using FFmpeg swresample
pub struct AudioResampler<I, O> {
    _p: PhantomData<O>,
    tx: Sender<Vec<I>>,
    status: Arc<AtomicBool>,
}

impl<I, O> AudioResampler<I, O>
where
    I: Copy + Send + 'static,
    O: Copy + Default,
{
    pub fn new<T: AudioResamplerOutput<O> + 'static>(
        input: AudioSampleDescription,
        output: AudioSampleDescription,
        mut sink: T,
    ) -> Result<Self, AudioResamplerError> {
        let (tx, rx) = channel::<Vec<I>>();

        let status = Arc::new(AtomicBool::new(true));
        let status_ = status.clone();

        thread::spawn(move || {
            // Create swresample context
            let mut swr_ctx: *mut SwrContext = null_mut();
            
            unsafe {
                swr_alloc_set_opts2(
                    &mut swr_ctx,
                    &output.channel_layout(),
                    output.sample_bits.into(),
                    output.sample_rate as i32,
                    &input.channel_layout(),
                    input.sample_bits.into(),
                    input.sample_rate as i32,
                    0,
                    null_mut(),
                );
                
                swr_init(swr_ctx);
            }

            while let Ok(buffer) = rx.recv() {
                let input_frames = buffer.len() / input.channels as usize;
                let output_frames = unsafe {
                    swr_get_out_samples(swr_ctx, input_frames as i32) as usize
                        / output.channels as usize
                };

                let mut output: Vec<O> = vec![O::default(); output_frames * output.channels as usize];

                let result = unsafe {
                    swr_convert(
                        swr_ctx,
                        output.as_mut_ptr().cast(),
                        output_frames as i32,
                        buffer.as_ptr().cast(),
                        input_frames as i32,
                    )
                };

                if result < 0 {
                    break;
                }

                if !sink.output(&output, result as u32) {
                    break;
                }
            }

            unsafe {
                swr_free(&mut swr_ctx);
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
