use crate::{AudioCaptureSourceDescription, CaptureHandler, FrameConsumer, Source, SourceType};

use std::{
    sync::{
        Arc, LazyLock,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};

use common::frame::AudioFrame;
use cpal::{
    Host, SampleFormat, Stream, StreamConfig,
    traits::{DeviceTrait, HostTrait, StreamTrait},
};
use parking_lot::Mutex;
use pipewire as pw;
use pw::spa::{
    self,
    param::{
        audio::{AudioFormat, AudioInfoRaw},
        format::{MediaSubtype, MediaType},
        format_utils,
    },
    pod::Pod,
};
use resample::{
    AudioResampler, AudioResamplerError, AudioResamplerOutput, AudioSampleDescription,
    AudioSampleFormat,
};
use thiserror::Error;

static HOST: LazyLock<Host> = LazyLock::new(cpal::default_host);

#[derive(Debug, Error)]
pub enum AudioCaptureError {
    #[error("Audio source not found: {0}")]
    NotFoundAudioSource(String),
    #[error("PipeWire audio error: {0}")]
    PipeWireError(String),
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

#[derive(Default)]
pub struct PipeWireAudioCapture {
    running: Arc<AtomicBool>,
    stream: Arc<Mutex<Option<Stream>>>,
    pw_thread: Arc<Mutex<Option<thread::JoinHandle<()>>>>,
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

        // Always expose a PipeWire system audio monitor entry first so desktop playback
        // can be captured natively via PipeWire STREAM_CAPTURE_SINK without guessing ALSA cards.
        sources.push(Source {
            id: "output:pipewire-monitor".to_string(),
            name: "System Audio (Desktop Monitor)".to_string(),
            index: idx,
            kind: SourceType::Audio,
            is_default: true,
        });
        idx += 1;

        // Add additional CPAL output devices if enumerated
        if let Ok(outputs) = HOST.output_devices() {
            for device in outputs {
                if let Ok(name) = device.name() {
                    let is_default_out = Some(&name) == default_output_name.as_ref();
                    if is_default_out && name == "default" {
                        continue;
                    }
                    sources.push(Source {
                        id: format!("output:{}", name),
                        name: format!("{} (Monitor)", name),
                        index: idx,
                        kind: SourceType::Audio,
                        is_default: false,
                    });
                    idx += 1;
                }
            }
        }

        // Add Microphone Inputs
        if let Ok(inputs) = HOST.input_devices() {
            for device in inputs {
                if let Ok(name) = device.name() {
                    let _ = &default_input_name;
                    sources.push(Source {
                        id: format!("input:{}", name),
                        name: format!("{} (Microphone)", name),
                        index: idx,
                        kind: SourceType::Audio,
                        is_default: false,
                    });
                    idx += 1;
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
        self.stop()?;

        let target_rate = options.sample_rate.max(8000);
        let want_monitor =
            options.source.id.starts_with("output:") || options.source.id == "default";

        self.running.store(true, Ordering::SeqCst);

        if want_monitor {
            match start_pipewire_monitor_thread(
                self.running.clone(),
                options.source.id.clone(),
                target_rate,
                consumer,
            ) {
                Ok(handle) => {
                    *self.pw_thread.lock() = Some(handle);
                    log::info!(
                        "Linux PipeWire sink monitor capture started ({} Hz stereo)",
                        target_rate
                    );
                    return Ok(());
                }
                Err((err, recovered_consumer)) => {
                    log::warn!(
                        "PipeWire sink monitor failed ({}); falling back to CPAL capture",
                        err
                    );
                    if let Some(recovered) = recovered_consumer {
                        return self.start_cpal_capture(options, target_rate, recovered);
                    }
                    return Err(err);
                }
            }
        }

        self.start_cpal_capture(options, target_rate, consumer)
    }

    fn stop(&self) -> Result<(), Self::Error> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(stream) = self.stream.lock().take() {
            let _ = stream.pause();
        }
        if let Some(handle) = self.pw_thread.lock().take() {
            let _ = handle.join();
        }
        log::info!("Linux audio capture stopped");
        Ok(())
    }
}

impl PipeWireAudioCapture {
    fn start_cpal_capture<S: FrameConsumer<Frame = AudioFrame> + 'static>(
        &self,
        options: AudioCaptureSourceDescription,
        target_rate: u32,
        consumer: S,
    ) -> Result<(), AudioCaptureError> {
        let device = if let Some(name) = options.source.id.strip_prefix("input:") {
            HOST.input_devices()?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .or_else(|| HOST.default_input_device())
        } else if let Some(name) = options.source.id.strip_prefix("output:") {
            HOST.output_devices()?
                .find(|d| d.name().map(|n| n == name).unwrap_or(false))
                .or_else(|| HOST.default_output_device())
        } else {
            HOST.default_input_device()
                .or_else(|| HOST.default_output_device())
        }
        .ok_or_else(|| AudioCaptureError::NotFoundAudioSource(options.source.name.clone()))?;

        let supported_config = device
            .default_input_config()
            .or_else(|_| device.default_output_config())?;

        let sample_format = supported_config.sample_format();
        let mut config: StreamConfig = supported_config.into();
        config.channels = 2;

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

        log::info!("Linux CPAL audio capture started: {} Hz stereo", target_rate);
        Ok(())
    }
}

struct PwAudioState {
    format: AudioInfoRaw,
    resampler: Option<AudioResampler<i16, i16>>,
    consumer_slot_taken: bool,
    target_rate: u32,
    i16_stereo: Vec<i16>,
}

fn start_pipewire_monitor_thread<S: FrameConsumer<Frame = AudioFrame> + 'static>(
    running: Arc<AtomicBool>,
    source_id: String,
    target_rate: u32,
    consumer: S,
) -> Result<thread::JoinHandle<()>, (AudioCaptureError, Option<S>)> {
    let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), AudioCaptureError>>(1);
    let consumer_slot = Arc::new(Mutex::new(Some(consumer)));
    let consumer_slot_thread = consumer_slot.clone();

    let running_thread = running.clone();

    let handle = match thread::Builder::new()
        .name("PipeWireAudioMonitor".to_string())
        .spawn(move || {
            pw::init();

            let Ok(mainloop) = pw::main_loop::MainLoopRc::new(None) else {
                let _ = init_tx.send(Err(AudioCaptureError::PipeWireError(
                    "MainLoop init failed".into(),
                )));
                return;
            };
            let Ok(context) = pw::context::ContextRc::new(&mainloop, None) else {
                let _ = init_tx.send(Err(AudioCaptureError::PipeWireError(
                    "Context init failed".into(),
                )));
                return;
            };
            let Ok(core) = context.connect_rc(None) else {
                let _ = init_tx.send(Err(AudioCaptureError::PipeWireError(
                    "Core connect failed".into(),
                )));
                return;
            };

            let mut props = pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
            };
            if let Some(target_name) = source_id.strip_prefix("output:")
                && (target_name.starts_with("alsa_output.")
                    || target_name.starts_with("bluez_output.")
                    || target_name.chars().all(|c| c.is_ascii_digit()))
            {
                props.insert("target.object", target_name);
            }

            let Ok(stream) = pw::stream::StreamBox::new(&core, "nayutacast-audio-monitor", props)
            else {
                let _ = init_tx.send(Err(AudioCaptureError::PipeWireError(
                    "StreamBox::new failed".into(),
                )));
                return;
            };

            let user_data = PwAudioState {
                format: AudioInfoRaw::new(),
                resampler: None,
                consumer_slot_taken: false,
                target_rate,
                i16_stereo: Vec::with_capacity(4096),
            };

            let ml = mainloop.clone();
            let err_flag = Arc::new(AtomicBool::new(false));
            let err_flag_cb = err_flag.clone();
            let running_cb = running_thread.clone();
            let slot_cb = consumer_slot_thread.clone();

            let Ok(_listener) = stream
                .add_local_listener_with_user_data(user_data)
                .state_changed(move |_, _, _, new| {
                    if matches!(new, pw::stream::StreamState::Error(_)) {
                        err_flag_cb.store(true, Ordering::SeqCst);
                    }
                })
                .param_changed(move |_, state, id, param| {
                    let Some(param) = param else {
                        return;
                    };
                    if id != spa::param::ParamType::Format.as_raw() {
                        return;
                    }
                    let Ok((media_type, media_subtype)) = format_utils::parse_format(param) else {
                        return;
                    };
                    if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                        return;
                    }
                    if state.format.parse(param).is_err() {
                        return;
                    }
                    let src_rate = state.format.rate().max(8000);
                    let src_ch = state.format.channels().max(1);
                    if !state.consumer_slot_taken
                        && let Some(consumer) = slot_cb.lock().take()
                    {
                        state.consumer_slot_taken = true;
                        let mut frame = AudioFrame::default();
                        frame.sample_rate = state.target_rate;
                        match AudioResampler::<i16, i16>::new(
                            AudioSampleDescription {
                                sample_bits: AudioSampleFormat::I16,
                                sample_rate: src_rate,
                                channels: 2,
                            },
                            AudioSampleDescription {
                                sample_bits: AudioSampleFormat::I16,
                                sample_rate: state.target_rate,
                                channels: 2,
                            },
                            Output { consumer, frame },
                        ) {
                            Ok(r) => {
                                state.resampler = Some(r);
                                log::info!(
                                    "PipeWire audio monitor format negotiated: {} Hz, {} ch -> {} Hz stereo",
                                    src_rate,
                                    src_ch,
                                    state.target_rate
                                );
                            }
                            Err(e) => {
                                log::error!("PipeWire AudioResampler init failed: {:?}", e);
                            }
                        }
                    }
                })
                .process(move |stream, state| {
                    if !running_cb.load(Ordering::SeqCst) {
                        ml.quit();
                        return;
                    }
                    if let Some(mut buffer) = stream.dequeue_buffer() {
                        let datas = buffer.datas_mut();
                        if datas.is_empty() {
                            return;
                        }
                        let offset = datas[0].chunk().offset() as usize;
                        let size = datas[0].chunk().size() as usize;
                        let channels = state.format.channels().max(1) as usize;
                        let Some(resampler) = state.resampler.as_mut() else {
                            return;
                        };
                        if let Some(raw) = datas[0].data()
                            && size >= 4
                            && offset.saturating_add(size) <= raw.len()
                        {
                            let bytes = &raw[offset..offset + size];
                            let n_floats = bytes.len() / 4;
                            let n_frames = n_floats / channels;
                            state.i16_stereo.clear();
                            for f_idx in 0..n_frames {
                                let base = f_idx * channels * 4;
                                let l = f32::from_le_bytes(
                                    bytes[base..base + 4].try_into().unwrap(),
                                );
                                let r = if channels >= 2 {
                                    f32::from_le_bytes(
                                        bytes[base + 4..base + 8].try_into().unwrap(),
                                    )
                                } else {
                                    l
                                };
                                state
                                    .i16_stereo
                                    .push((l * 32767.0).clamp(-32768.0, 32767.0) as i16);
                                state
                                    .i16_stereo
                                    .push((r * 32767.0).clamp(-32768.0, 32767.0) as i16);
                            }
                            if !state.i16_stereo.is_empty() {
                                let _ = resampler.resample(&state.i16_stereo);
                            }
                        }
                    }
                })
                .register()
            else {
                let _ = init_tx.send(Err(AudioCaptureError::PipeWireError(
                    "Stream listener register failed".into(),
                )));
                return;
            };

            let mut audio_info = AudioInfoRaw::new();
            audio_info.set_format(AudioFormat::F32LE);
            let obj = spa::pod::Object {
                type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
                id: spa::param::ParamType::EnumFormat.as_raw(),
                properties: audio_info.into(),
            };
            let values: Vec<u8> = spa::pod::serialize::PodSerializer::serialize(
                std::io::Cursor::new(Vec::new()),
                &spa::pod::Value::Object(obj),
            )
            .unwrap()
            .0
            .into_inner();
            let mut params = [Pod::from_bytes(&values).unwrap()];

            if let Err(e) = stream.connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT
                    | pw::stream::StreamFlags::MAP_BUFFERS
                    | pw::stream::StreamFlags::RT_PROCESS,
                &mut params,
            ) {
                let _ = init_tx.send(Err(AudioCaptureError::PipeWireError(format!(
                    "stream.connect failed: {}",
                    e
                ))));
                return;
            }

            let _ = init_tx.send(Ok(()));

            while running_thread.load(Ordering::SeqCst) && !err_flag.load(Ordering::SeqCst) {
                mainloop
                    .loop_()
                    .iterate(pw::loop_::Timeout::Finite(Duration::from_millis(20)));
            }
        }) {
        Ok(h) => h,
        Err(e) => {
            return Err((
                AudioCaptureError::PipeWireError(format!("spawn failed: {}", e)),
                consumer_slot.lock().take(),
            ));
        }
    };

    match init_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(err)) => {
            let _ = handle.join();
            Err((err, consumer_slot.lock().take()))
        }
        Err(_) => {
            running.store(false, Ordering::SeqCst);
            let _ = handle.join();
            Err((
                AudioCaptureError::PipeWireError(
                    "PipeWire audio monitor thread init timed out".into(),
                ),
                consumer_slot.lock().take(),
            ))
        }
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
