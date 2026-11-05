use crate::{AudioCaptureSourceDescription, CaptureHandler, FrameConsumer, Source, SourceType};

use std::sync::{
    Arc, LazyLock,
    atomic::{AtomicBool, Ordering},
};

use common::frame::AudioFrame;
use cpal::{
    Host, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use parking_lot::Mutex;
use resample::{
    AudioResampler, AudioResamplerError, AudioResamplerOutput, AudioSampleDescription,
    AudioSampleFormat,
};
use thiserror::Error;

static HOST: LazyLock<Host> = LazyLock::new(|| cpal::default_host());

#[derive(Debug, Error)]
pub enum AudioCaptureError {
    #[error("Audio source not found: {0}")]
    NotFoundAudioSource(String),
    #[error("CPAL device error: {0}")]
    DeviceError(#[from] cpal::DevicesError),
    #[error("CPAL device name error: {0}")]
    DeviceNameError(#[from] cpal::DeviceNameError),
    #[error("CPAL stream config error: {0}")]
    DefaultStreamConfigError(#[from] cpal::DefaultStreamConfigError),
    #[error("CPAL build stream error: {0}")]
    BuildStreamError(#[from] cpal::BuildStreamError),
    #[error("CPAL play stream error: {0}")]
    PlayStreamError(#[from] cpal::PlayStreamError),
    #[error("Audio resampling error: {0}")]
    ResampleError(#[from] AudioResamplerError),
}

enum DeviceKind {
    Input,
    Output,
}

#[derive(Default)]
pub struct PipeWireAudioCapture {
    running: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<Stream>>>,
}

unsafe impl Send for PipeWireAudioCapture {}
unsafe impl Sync for PipeWireAudioCapture {}

impl CaptureHandler for PipeWireAudioCapture {
    type Frame = AudioFrame;
    type Error = AudioCaptureError;
    type CaptureOptions = AudioCaptureSourceDescription;

    fn get_sources() -> Result<Vec<Source>, Self::Error> {
        let default_input_name = HOST.default_input_device().and_then(|d| d.name().ok());
        let default_output_name = HOST.default_output_device().and_then(|d| d.name().ok());

        let mut sources = Vec::new();
        let mut idx = 0;

        // Add System Audio Output / Monitor
        if let Ok(outputs) = HOST.output_devices() {
            for device in outputs {
                if let Ok(name) = device.name() {
                    let is_default = Some(&name) == default_output_name.as_ref();
                    sources.push(Source {
                        id: format!("output:{}", name),
                        name: format!("{} (Monitor)", name),
                        index: idx,
                        kind: SourceType::Audio,
                        is_default,
                    });
                    idx += 1;
                }
            }
        }

        // Add Microphone Inputs
        if let Ok(inputs) = HOST.input_devices() {
            for device in inputs {
                if let Ok(name) = device.name() {
                    let is_default = sources.is_empty() && (Some(&name) == default_input_name.as_ref());
                    sources.push(Source {
                        id: format!("input:{}", name),
                        name: format!("{} (Microphone)", name),
                        index: idx,
                        kind: SourceType::Audio,
                        is_default,
                    });
                    idx += 1;
                }
            }
        }

        if sources.is_empty() {
            sources.push(Source {
                id: "default".to_string(),
                name: "Default Audio Source".to_string(),
                index: 0,
                kind: SourceType::Audio,
                is_default: true,
            });
        }

        Ok(sources)
    }

    fn start<S: FrameConsumer<Frame = Self::Frame> + 'static>(
        &self,
        options: Self::CaptureOptions,
        consumer: S,
    ) -> Result<(), Self::Error> {
        let (device, _kind) = if options.source.id.starts_with("output:") {
            let name = &options.source.id["output:".len()..];
            HOST.output_devices()?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .map(|d| (d, DeviceKind::Output))
                .or_else(|| HOST.default_output_device().map(|d| (d, DeviceKind::Output)))
        } else if options.source.id.starts_with("input:") {
            let name = &options.source.id["input:".len()..];
            HOST.input_devices()?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .map(|d| (d, DeviceKind::Input))
                .or_else(|| HOST.default_input_device().map(|d| (d, DeviceKind::Input)))
        } else {
            HOST.default_input_device()
                .map(|d| (d, DeviceKind::Input))
                .or_else(|| HOST.default_output_device().map(|d| (d, DeviceKind::Output)))
        }
        .ok_or_else(|| AudioCaptureError::NotFoundAudioSource(options.source.name.clone()))?;

        let supported_config = device.default_input_config()
            .or_else(|_| device.default_output_config())?;

        let sample_format = supported_config.sample_format();
        let mut config: StreamConfig = supported_config.into();
        config.channels = 2;

        let target_rate = options.sample_rate.max(8000);
        let mut frame = AudioFrame::default();
        frame.sample_rate = target_rate;

        let mut resampler = AudioResampler::new(
            AudioSampleDescription {
                sample_bits: AudioSampleFormat::I16,
                sample_rate: config.sample_rate.0,
                channels: 2,
            },
            AudioSampleDescription {
                sample_bits: AudioSampleFormat::I16,
                sample_rate: target_rate,
                channels: 2,
            },
            Output { consumer, frame },
        )?;

        self.running.store(true, Ordering::SeqCst);
        let running = self.running.clone();

        let err_fn = |err| {
            log::error!("Audio capture stream error: {:?}", err);
        };

        let stream = match sample_format {
            SampleFormat::I16 => device.build_input_stream(
                &config,
                move |data: &[i16], _| {
                    if running.load(Ordering::SeqCst) {
                        let _ = resampler.resample(data);
                    }
                },
                err_fn,
                None,
            )?,
            SampleFormat::F32 => {
                let mut i16_buf = Vec::new();
                device.build_input_stream(
                    &config,
                    move |data: &[f32], _| {
                        if running.load(Ordering::SeqCst) {
                            i16_buf.clear();
                            i16_buf.extend(data.iter().map(|&s| {
                                (s * 32767.0).clamp(-32768.0, 32767.0) as i16
                            }));
                            let _ = resampler.resample(&i16_buf);
                        }
                    },
                    err_fn,
                    None,
                )?
            }
            _ => {
                return Err(AudioCaptureError::NotFoundAudioSource(
                    "Unsupported audio sample format".to_string(),
                ));
            }
        };

        stream.play()?;
        *self.stream.lock() = Some(stream);

        log::info!("Linux audio capture started: {} Hz stereo", target_rate);
        Ok(())
    }

    fn stop(&self) -> Result<(), Self::Error> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(stream) = self.stream.lock().take() {
            let _ = stream.pause();
        }
        log::info!("Linux audio capture stopped");
        Ok(())
    }
}

struct Output<S> {
    consumer: S,
    frame: AudioFrame,
}

impl<S> AudioResamplerOutput<i16> for Output<S>
where
    S: FrameConsumer<Frame = AudioFrame> + 'static,
{
    fn output(&mut self, buffer: &[i16], frames: u32) -> bool {
        self.frame.data = buffer.as_ptr();
        self.frame.frames = frames;
        self.consumer.sink(&self.frame)
    }
}
