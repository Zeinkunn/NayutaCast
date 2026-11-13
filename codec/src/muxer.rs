use std::{
    collections::VecDeque,
    ffi::{CString, c_int, c_void},
    ptr::null_mut,
};

use common::{
    Size,
    codec::VideoEncoderType,
    frame::{AudioFrame, VideoFormat, VideoFrame, VideoSubFormat},
    strings::PSTR,
};
use ffmpeg::*;
use thiserror::Error;

use crate::codec::{CodecType, create_video_context, set_option, set_str_option};

const VIDEO_TIMEBASE_DEN: i32 = 90_000;
const HLS_MAX_SEGMENTS: usize = 6;
const IO_BUFFER_SIZE: usize = 65_536;

#[derive(Debug, Error)]
pub enum LiveMuxerError {
    #[error("video encoder setup failed: {0}")]
    VideoEncoder(String),
    #[error("audio encoder setup failed: {0}")]
    AudioEncoder(String),
    #[error("muxer container '{format}' setup failed: {detail}")]
    Container { format: &'static str, detail: String },
    #[error("encode error: {0}")]
    Encode(String),
}

#[derive(Debug, Clone)]
pub struct LiveMuxerOptions {
    pub width: u32,
    pub height: u32,
    pub fps: u8,
    pub video_bitrate: u64,
    pub audio_sample_rate: u32,
    pub audio_bitrate: u64,
    pub encoder_type: VideoEncoderType,
}

impl Default for LiveMuxerOptions {
    fn default() -> Self {
        Self {
            width: 1280,
            height: 720,
            fps: 30,
            video_bitrate: 4_000_000,
            audio_sample_rate: 48_000,
            audio_bitrate: 128_000,
            encoder_type: VideoEncoderType::X264,
        }
    }
}

#[derive(Debug, Default)]
pub struct MuxerOutput {
    pub mpegts_chunks: Vec<Vec<u8>>,
    pub mpegts_is_keyframe: bool,
    pub mp4_fragment: Option<Vec<u8>>,
}

struct IoBufferState {
    bytes: Vec<u8>,
}

unsafe extern "C" fn io_write_packet(
    opaque: *mut c_void,
    buf: *const u8,
    buf_size: c_int,
) -> c_int {
    if opaque.is_null() || buf.is_null() || buf_size <= 0 {
        return buf_size;
    }
    let state = unsafe { &mut *(opaque as *mut IoBufferState) };
    let slice = unsafe { std::slice::from_raw_parts(buf, buf_size as usize) };
    state.bytes.extend_from_slice(slice);
    buf_size
}

struct ContainerMuxer {
    format_name: &'static str,
    fmt_ctx: *mut AVFormatContext,
    avio_ctx: *mut AVIOContext,
    io_state: *mut IoBufferState,
    video_stream_idx: c_int,
    audio_stream_idx: c_int,
    header_written: bool,
}

impl ContainerMuxer {
    fn new(
        format_name: &'static str,
        video_ctx: *const AVCodecContext,
        audio_ctx: *const AVCodecContext,
        mux_opts: &[(&str, &str)],
    ) -> Result<(Self, Vec<u8>), LiveMuxerError> {
        let fmt_c = CString::new(format_name).unwrap();
        let mut fmt_ctx: *mut AVFormatContext = null_mut();
        let rc = unsafe {
            avformat_alloc_output_context2(
                &mut fmt_ctx,
                null_mut(),
                fmt_c.as_ptr(),
                std::ptr::null(),
            )
        };
        if rc < 0 || fmt_ctx.is_null() {
            return Err(LiveMuxerError::Container {
                format: format_name,
                detail: format!("avformat_alloc_output_context2 failed ({})", rc),
            });
        }

        let io_state = Box::into_raw(Box::new(IoBufferState {
            bytes: Vec::with_capacity(65_536),
        }));
        let io_buf = unsafe { av_malloc(IO_BUFFER_SIZE) as *mut u8 };
        if io_buf.is_null() {
            unsafe {
                drop(Box::from_raw(io_state));
                avformat_free_context(fmt_ctx);
            }
            return Err(LiveMuxerError::Container {
                format: format_name,
                detail: "av_malloc failed".into(),
            });
        }

        let avio_ctx = unsafe {
            avio_alloc_context(
                io_buf,
                IO_BUFFER_SIZE as c_int,
                1,
                io_state as *mut c_void,
                None,
                Some(io_write_packet),
                None,
            )
        };
        if avio_ctx.is_null() {
            unsafe {
                av_free(io_buf as *mut c_void);
                drop(Box::from_raw(io_state));
                avformat_free_context(fmt_ctx);
            }
            return Err(LiveMuxerError::Container {
                format: format_name,
                detail: "avio_alloc_context failed".into(),
            });
        }

        unsafe {
            (*fmt_ctx).pb = avio_ctx;
            (*fmt_ctx).flags |= AVFMT_FLAG_CUSTOM_IO as i32;
        }

        let v_stream = unsafe { avformat_new_stream(fmt_ctx, std::ptr::null()) };
        let a_stream = unsafe { avformat_new_stream(fmt_ctx, std::ptr::null()) };
        if v_stream.is_null() || a_stream.is_null() {
            let mut this = Self {
                format_name,
                fmt_ctx,
                avio_ctx,
                io_state,
                video_stream_idx: 0,
                audio_stream_idx: 1,
                header_written: false,
            };
            this.header_written = false;
            return Err(LiveMuxerError::Container {
                format: format_name,
                detail: "avformat_new_stream failed".into(),
            });
        }

        unsafe {
            if avcodec_parameters_from_context((*v_stream).codecpar, video_ctx) < 0 {
                return Err(LiveMuxerError::Container {
                    format: format_name,
                    detail: "copy video codecpar failed".into(),
                });
            }
            (*v_stream).time_base = (*video_ctx).time_base;

            if avcodec_parameters_from_context((*a_stream).codecpar, audio_ctx) < 0 {
                return Err(LiveMuxerError::Container {
                    format: format_name,
                    detail: "copy audio codecpar failed".into(),
                });
            }
            (*a_stream).time_base = (*audio_ctx).time_base;
        }

        let mut opts: *mut AVDictionary = null_mut();
        for (k, v) in mux_opts {
            let ck = CString::new(*k).unwrap();
            let cv = CString::new(*v).unwrap();
            unsafe {
                av_dict_set(&mut opts, ck.as_ptr(), cv.as_ptr(), 0);
            }
        }

        let hdr_rc = unsafe { avformat_write_header(fmt_ctx, &mut opts) };
        unsafe {
            av_dict_free(&mut opts);
        }

        let mut this = Self {
            format_name,
            fmt_ctx,
            avio_ctx,
            io_state,
            video_stream_idx: unsafe { (*v_stream).index },
            audio_stream_idx: unsafe { (*a_stream).index },
            header_written: hdr_rc >= 0,
        };

        if hdr_rc < 0 {
            return Err(LiveMuxerError::Container {
                format: format_name,
                detail: format!("avformat_write_header returned {}", hdr_rc),
            });
        }

        unsafe {
            avio_flush(this.avio_ctx);
        }
        let init_bytes = this.take_bytes();
        Ok((this, init_bytes))
    }

    fn write_packet(
        &mut self,
        src_pkt: *const AVPacket,
        src_tb: AVRational,
        is_video: bool,
    ) -> Result<Vec<u8>, LiveMuxerError> {
        let cloned = unsafe { av_packet_clone(src_pkt) };
        if cloned.is_null() {
            return Err(LiveMuxerError::Container {
                format: self.format_name,
                detail: "av_packet_clone failed".into(),
            });
        }

        let stream_idx = if is_video {
            self.video_stream_idx
        } else {
            self.audio_stream_idx
        };

        unsafe {
            let dst_stream = *(*self.fmt_ctx).streams.add(stream_idx as usize);
            av_packet_rescale_ts(cloned, src_tb, (*dst_stream).time_base);
            (*cloned).stream_index = stream_idx;
            (*cloned).pos = -1;
        }

        let rc = unsafe { av_interleaved_write_frame(self.fmt_ctx, cloned) };
        unsafe {
            let mut ptr = cloned;
            av_packet_free(&mut ptr);
        }
        if rc < 0 {
            return Err(LiveMuxerError::Container {
                format: self.format_name,
                detail: format!("av_interleaved_write_frame failed ({})", rc),
            });
        }

        unsafe {
            avio_flush(self.avio_ctx);
        }
        Ok(self.take_bytes())
    }

    fn take_bytes(&mut self) -> Vec<u8> {
        let state = unsafe { &mut *self.io_state };
        if state.bytes.is_empty() {
            Vec::new()
        } else {
            std::mem::take(&mut state.bytes)
        }
    }
}

impl Drop for ContainerMuxer {
    fn drop(&mut self) {
        if !self.fmt_ctx.is_null() {
            if self.header_written {
                unsafe {
                    av_write_trailer(self.fmt_ctx);
                    if !self.avio_ctx.is_null() {
                        avio_flush(self.avio_ctx);
                    }
                }
            }
            unsafe {
                (*self.fmt_ctx).pb = null_mut();
                avformat_free_context(self.fmt_ctx);
            }
            self.fmt_ctx = null_mut();
        }
        if !self.avio_ctx.is_null() {
            unsafe {
                if !(*self.avio_ctx).buffer.is_null() {
                    av_freep(&mut (*self.avio_ctx).buffer as *mut _ as *mut c_void);
                }
                avio_context_free(&mut self.avio_ctx);
            }
        }
        if !self.io_state.is_null() {
            unsafe {
                drop(Box::from_raw(self.io_state));
            }
            self.io_state = null_mut();
        }
    }
}

/// Encodes video (H.264 Constrained Baseline/Main) and audio (AAC-LC) once and simultaneously
/// produces MPEG-TS chunks, rolling in-memory HLS segments/playlist, and fragmented MP4 chunks.
pub struct UniversalLiveMuxer {
    options: LiveMuxerOptions,
    // Video encoder
    v_ctx: *mut AVCodecContext,
    v_frame: *mut AVFrame,
    v_sw_frame: *mut AVFrame,
    v_pkt: *mut AVPacket,
    v_frame_idx: i64,
    v_using_vaapi: bool,
    // Audio encoder
    a_ctx: *mut AVCodecContext,
    a_frame: *mut AVFrame,
    a_pkt: *mut AVPacket,
    a_frame_size: usize,
    a_fifo_l: Vec<f32>,
    a_fifo_r: Vec<f32>,
    a_samples_pts: i64,
    // Muxers & buffers
    ts_muxer: ContainerMuxer,
    mp4_muxer: ContainerMuxer,
    fmp4_init: Vec<u8>,
    latest_mp4_fragment: Option<Vec<u8>>,
    mpegts_gop_buf: Vec<u8>,
    // HLS state
    hls_cur_buf: Vec<u8>,
    hls_cur_video_frames: u64,
    hls_next_seq: u64,
    hls_segments: VecDeque<(u64, f64, Vec<u8>)>,
    hls_playlist: String,
}

unsafe impl Send for UniversalLiveMuxer {}
unsafe impl Sync for UniversalLiveMuxer {}

impl UniversalLiveMuxer {
    pub fn new(options: LiveMuxerOptions) -> Result<Self, LiveMuxerError> {
        let mut opts = options;
        opts.width = (opts.width & !1).max(64);
        opts.height = (opts.height & !1).max(64);
        opts.fps = opts.fps.clamp(10, 60);
        opts.audio_sample_rate = if opts.audio_sample_rate == 44_100 {
            44_100
        } else {
            48_000
        };

        let (v_ctx, v_frame, v_sw_frame, v_using_vaapi) = Self::open_video_encoder(&opts)?;
        let (a_ctx, a_frame, a_frame_size) = Self::open_audio_encoder(&opts)?;

        let v_pkt = unsafe { av_packet_alloc() };
        let a_pkt = unsafe { av_packet_alloc() };
        if v_pkt.is_null() || a_pkt.is_null() {
            return Err(LiveMuxerError::Container {
                format: "init",
                detail: "av_packet_alloc failed".into(),
            });
        }

        let (ts_muxer, ts_init) = ContainerMuxer::new(
            "mpegts",
            v_ctx,
            a_ctx,
            &[("mpegts_flags", "resend_headers"), ("flush_packets", "1")],
        )?;

        let (mp4_muxer, fmp4_init) = ContainerMuxer::new(
            "mp4",
            v_ctx,
            a_ctx,
            &[
                (
                    "movflags",
                    "frag_keyframe+empty_moov+default_base_moof+omit_tfhd_offset",
                ),
                ("flush_packets", "1"),
            ],
        )?;

        let hls_playlist = Self::render_playlist(&VecDeque::new(), opts.fps);

        Ok(Self {
            options: opts,
            v_ctx,
            v_frame,
            v_sw_frame,
            v_pkt,
            v_frame_idx: 0,
            v_using_vaapi,
            a_ctx,
            a_frame,
            a_pkt,
            a_frame_size,
            a_fifo_l: Vec::with_capacity(a_frame_size * 4),
            a_fifo_r: Vec::with_capacity(a_frame_size * 4),
            a_samples_pts: 0,
            ts_muxer,
            mp4_muxer,
            fmp4_init,
            latest_mp4_fragment: None,
            mpegts_gop_buf: ts_init.clone(),
            hls_cur_buf: ts_init,
            hls_cur_video_frames: 0,
            hls_next_seq: 0,
            hls_segments: VecDeque::with_capacity(HLS_MAX_SEGMENTS + 1),
            hls_playlist,
        })
    }

    fn open_video_encoder(
        opts: &LiveMuxerOptions,
    ) -> Result<(*mut AVCodecContext, *mut AVFrame, *mut AVFrame, bool), LiveMuxerError> {
        if cfg!(target_os = "linux") && opts.encoder_type == VideoEncoderType::Vaapi {
            if let Ok(res) = Self::try_open_video_variant(opts, VideoEncoderType::Vaapi) {
                return Ok(res);
            }
            log::warn!("LiveMuxer VAAPI init failed; falling back to libx264");
        }
        Self::try_open_video_variant(opts, VideoEncoderType::X264)
    }

    fn try_open_video_variant(
        opts: &LiveMuxerOptions,
        codec_kind: VideoEncoderType,
    ) -> Result<(*mut AVCodecContext, *mut AVFrame, *mut AVFrame, bool), LiveMuxerError> {
        let mut ctx: *mut AVCodecContext = null_mut();
        let codec = if cfg!(target_os = "linux") && codec_kind == VideoEncoderType::Vaapi {
            create_video_context(
                &mut ctx,
                CodecType::Encoder(VideoEncoderType::Vaapi),
                Some(Size {
                    width: opts.width,
                    height: opts.height,
                }),
            )
            .map_err(|e| LiveMuxerError::VideoEncoder(e.to_string()))?
        } else {
            let c = unsafe { avcodec_find_encoder_by_name(PSTR::from("libx264").as_ptr()) };
            let c = if c.is_null() {
                unsafe { avcodec_find_encoder(AVCodecID::AV_CODEC_ID_H264) }
            } else {
                c
            };
            if c.is_null() {
                return Err(LiveMuxerError::VideoEncoder("H.264 encoder not found".into()));
            }
            ctx = unsafe { avcodec_alloc_context3(c) };
            if ctx.is_null() {
                return Err(LiveMuxerError::VideoEncoder("avcodec_alloc_context3 failed".into()));
            }
            c
        };

        let using_vaapi = cfg!(target_os = "linux") && codec_kind == VideoEncoderType::Vaapi;
        let ctx_mut = unsafe { &mut *ctx };
        ctx_mut.width = opts.width as i32;
        ctx_mut.height = opts.height as i32;
        ctx_mut.bit_rate = opts.video_bitrate.max(500_000) as i64;
        ctx_mut.rc_max_rate = ctx_mut.bit_rate;
        ctx_mut.rc_buffer_size = ctx_mut.bit_rate as i32;
        ctx_mut.framerate = unsafe { av_make_q(opts.fps as i32, 1) };
        ctx_mut.time_base = unsafe { av_make_q(1, VIDEO_TIMEBASE_DEN) };
        ctx_mut.pkt_timebase = unsafe { av_make_q(1, VIDEO_TIMEBASE_DEN) };
        ctx_mut.gop_size = (opts.fps as i32).max(15).min(30);
        ctx_mut.keyint_min = ctx_mut.gop_size;
        ctx_mut.max_b_frames = 0;
        ctx_mut.has_b_frames = 0;
        ctx_mut.profile = FF_PROFILE_H264_BASELINE as i32;
        ctx_mut.flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
        ctx_mut.pix_fmt = if using_vaapi {
            AVPixelFormat::AV_PIX_FMT_VAAPI
        } else {
            AVPixelFormat::AV_PIX_FMT_YUV420P
        };

        if using_vaapi {
            set_option(ctx_mut, "low_power", 1);
        } else {
            ctx_mut.thread_count = 4;
            ctx_mut.thread_type = FF_THREAD_SLICE as i32;
            set_str_option(ctx_mut, "preset", "ultrafast");
            set_str_option(ctx_mut, "tune", "zerolatency");
            set_str_option(ctx_mut, "profile", "baseline");
        }

        if unsafe { avcodec_open2(ctx, codec, null_mut()) } < 0 {
            unsafe { avcodec_free_context(&mut ctx) };
            return Err(LiveMuxerError::VideoEncoder("avcodec_open2 failed".into()));
        }

        let v_frame = unsafe { av_frame_alloc() };
        if v_frame.is_null() {
            unsafe { avcodec_free_context(&mut ctx) };
            return Err(LiveMuxerError::VideoEncoder("av_frame_alloc failed".into()));
        }
        unsafe {
            (*v_frame).width = opts.width as i32;
            (*v_frame).height = opts.height as i32;
            (*v_frame).format = ctx_mut.pix_fmt as i32;
        }

        let mut sw_frame: *mut AVFrame = null_mut();
        if using_vaapi {
            if unsafe { av_hwframe_get_buffer(ctx_mut.hw_frames_ctx, v_frame, 0) } < 0 {
                unsafe {
                    let mut vf = v_frame;
                    av_frame_free(&mut vf);
                    avcodec_free_context(&mut ctx);
                }
                return Err(LiveMuxerError::VideoEncoder(
                    "av_hwframe_get_buffer failed".into(),
                ));
            }
            sw_frame = unsafe { av_frame_alloc() };
            unsafe {
                (*sw_frame).width = opts.width as i32;
                (*sw_frame).height = opts.height as i32;
                (*sw_frame).format = AVPixelFormat::AV_PIX_FMT_NV12 as i32;
                if av_frame_get_buffer(sw_frame, 0) < 0 {
                    let mut sf = sw_frame;
                    let mut vf = v_frame;
                    av_frame_free(&mut sf);
                    av_frame_free(&mut vf);
                    avcodec_free_context(&mut ctx);
                    return Err(LiveMuxerError::VideoEncoder(
                        "sw staging frame alloc failed".into(),
                    ));
                }
            }
        } else if unsafe { av_frame_get_buffer(v_frame, 0) } < 0 {
            unsafe {
                let mut vf = v_frame;
                av_frame_free(&mut vf);
                avcodec_free_context(&mut ctx);
            }
            return Err(LiveMuxerError::VideoEncoder(
                "av_frame_get_buffer failed".into(),
            ));
        }

        Ok((ctx, v_frame, sw_frame, using_vaapi))
    }

    fn open_audio_encoder(
        opts: &LiveMuxerOptions,
    ) -> Result<(*mut AVCodecContext, *mut AVFrame, usize), LiveMuxerError> {
        let codec = unsafe { avcodec_find_encoder(AVCodecID::AV_CODEC_ID_AAC) };
        if codec.is_null() {
            return Err(LiveMuxerError::AudioEncoder("AAC encoder not found".into()));
        }
        let mut ctx = unsafe { avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err(LiveMuxerError::AudioEncoder("avcodec_alloc_context3 failed".into()));
        }

        let ctx_mut = unsafe { &mut *ctx };
        ctx_mut.sample_fmt = AVSampleFormat::AV_SAMPLE_FMT_FLTP;
        ctx_mut.sample_rate = opts.audio_sample_rate as i32;
        ctx_mut.bit_rate = opts.audio_bitrate.clamp(64_000, 320_000) as i64;
        ctx_mut.time_base = unsafe { av_make_q(1, opts.audio_sample_rate as i32) };
        ctx_mut.flags |= AV_CODEC_FLAG_GLOBAL_HEADER as i32;
        ctx_mut.ch_layout = AVChannelLayout {
            order: AVChannelOrder::AV_CHANNEL_ORDER_NATIVE,
            nb_channels: 2,
            u: AVChannelLayout__bindgen_ty_1 {
                mask: AV_CH_LAYOUT_STEREO,
            },
            opaque: null_mut(),
        };

        if unsafe { avcodec_open2(ctx, codec, null_mut()) } < 0 {
            unsafe { avcodec_free_context(&mut ctx) };
            return Err(LiveMuxerError::AudioEncoder("avcodec_open2 failed".into()));
        }

        let frame_size = (ctx_mut.frame_size.max(1024)) as usize;
        let a_frame = unsafe { av_frame_alloc() };
        if a_frame.is_null() {
            unsafe { avcodec_free_context(&mut ctx) };
            return Err(LiveMuxerError::AudioEncoder("av_frame_alloc failed".into()));
        }
        unsafe {
            (*a_frame).nb_samples = frame_size as i32;
            (*a_frame).format = AVSampleFormat::AV_SAMPLE_FMT_FLTP as i32;
            (*a_frame).ch_layout = ctx_mut.ch_layout;
            (*a_frame).sample_rate = ctx_mut.sample_rate;
            if av_frame_get_buffer(a_frame, 0) < 0 {
                let mut af = a_frame;
                av_frame_free(&mut af);
                avcodec_free_context(&mut ctx);
                return Err(LiveMuxerError::AudioEncoder(
                    "av_frame_get_buffer failed".into(),
                ));
            }
        }

        Ok((ctx, a_frame, frame_size))
    }

    pub fn fmp4_init_segment(&self) -> &[u8] {
        &self.fmp4_init
    }

    pub fn latest_mp4_fragment(&self) -> Option<&[u8]> {
        self.latest_mp4_fragment.as_deref()
    }

    pub fn mpegts_gop_prefix(&self) -> &[u8] {
        &self.mpegts_gop_buf
    }

    pub fn hls_playlist(&self) -> &str {
        &self.hls_playlist
    }

    pub fn hls_segment(&self, segment_name: &str) -> Option<&[u8]> {
        let stripped = segment_name
            .trim_start_matches('/')
            .strip_prefix("seg_")?
            .strip_suffix(".ts")?;
        let seq: u64 = stripped.parse().ok()?;
        self.hls_segments
            .iter()
            .find(|(s, _, _)| *s == seq)
            .map(|(_, _, data)| data.as_slice())
    }

    /// Pushes interleaved stereo S16 audio frames into the AAC FIFO and emits encoded packets.
    pub fn push_audio(&mut self, frame: &AudioFrame) -> Result<MuxerOutput, LiveMuxerError> {
        if frame.data.is_null() || frame.frames == 0 {
            return Ok(MuxerOutput::default());
        }
        let samples_i16 = unsafe {
            std::slice::from_raw_parts(frame.data as *const i16, (frame.frames as usize) * 2)
        };
        for pair in samples_i16.chunks_exact(2) {
            self.a_fifo_l.push(pair[0] as f32 / 32768.0);
            self.a_fifo_r.push(pair[1] as f32 / 32768.0);
        }
        let mut out = MuxerOutput::default();
        self.drain_audio_fifo(&mut out)?;
        Ok(out)
    }

    /// Pushes one video frame (NV12 or I420 SW), pads silence if audio lags behind video PTS,
    /// and returns newly produced MPEG-TS / fMP4 outputs.
    pub fn push_video(&mut self, frame: &VideoFrame) -> Result<MuxerOutput, LiveMuxerError> {
        if frame.sub_format != VideoSubFormat::SW || frame.data[0].is_null() {
            return Ok(MuxerOutput::default());
        }

        let mut out = MuxerOutput::default();
        let target_audio_samples = (self.v_frame_idx * self.options.audio_sample_rate as i64)
            / (self.options.fps as i64);
        while self.a_samples_pts + (self.a_fifo_l.len() as i64) < target_audio_samples {
            let need = (target_audio_samples - self.a_samples_pts) as usize;
            let chunk = need.clamp(self.a_frame_size, self.a_frame_size * 2);
            self.a_fifo_l
                .extend(std::iter::repeat_n(0.0f32, chunk.saturating_sub(self.a_fifo_l.len())));
            self.a_fifo_r
                .extend(std::iter::repeat_n(0.0f32, chunk.saturating_sub(self.a_fifo_r.len())));
            if self.a_fifo_l.len() < self.a_frame_size {
                let pad = self.a_frame_size - self.a_fifo_l.len();
                self.a_fifo_l.extend(std::iter::repeat_n(0.0f32, pad));
                self.a_fifo_r.extend(std::iter::repeat_n(0.0f32, pad));
            }
            self.drain_audio_fifo(&mut out)?;
        }

        self.upload_video_frame(frame)?;

        let pts = (self.v_frame_idx * VIDEO_TIMEBASE_DEN as i64) / (self.options.fps as i64);
        self.v_frame_idx += 1;

        unsafe {
            (*self.v_frame).pts = pts;
        }

        if unsafe { avcodec_send_frame(self.v_ctx, self.v_frame) } < 0 {
            return Err(LiveMuxerError::Encode("avcodec_send_frame video failed".into()));
        }

        loop {
            let rc = unsafe { avcodec_receive_packet(self.v_ctx, self.v_pkt) };
            if rc != 0 {
                break;
            }
            let is_key = unsafe { ((*self.v_pkt).flags & AV_PKT_FLAG_KEY as i32) != 0 };
            let tb = unsafe { (*self.v_ctx).time_base };
            self.mux_packet(self.v_pkt, tb, true, is_key, &mut out)?;
            unsafe {
                av_packet_unref(self.v_pkt);
            }
        }

        Ok(out)
    }

    fn drain_audio_fifo(&mut self, out: &mut MuxerOutput) -> Result<(), LiveMuxerError> {
        while self.a_fifo_l.len() >= self.a_frame_size && self.a_fifo_r.len() >= self.a_frame_size {
            if unsafe { av_frame_make_writable(self.a_frame) } < 0 {
                return Err(LiveMuxerError::Encode("audio frame not writable".into()));
            }
            unsafe {
                let af = &mut *self.a_frame;
                let dst_l = std::slice::from_raw_parts_mut(af.data[0] as *mut f32, self.a_frame_size);
                let dst_r = std::slice::from_raw_parts_mut(af.data[1] as *mut f32, self.a_frame_size);
                dst_l.copy_from_slice(&self.a_fifo_l[..self.a_frame_size]);
                dst_r.copy_from_slice(&self.a_fifo_r[..self.a_frame_size]);
                af.pts = self.a_samples_pts;
            }
            self.a_fifo_l.drain(..self.a_frame_size);
            self.a_fifo_r.drain(..self.a_frame_size);
            self.a_samples_pts += self.a_frame_size as i64;

            if unsafe { avcodec_send_frame(self.a_ctx, self.a_frame) } < 0 {
                return Err(LiveMuxerError::Encode("avcodec_send_frame audio failed".into()));
            }

            loop {
                let rc = unsafe { avcodec_receive_packet(self.a_ctx, self.a_pkt) };
                if rc != 0 {
                    break;
                }
                let tb = unsafe { (*self.a_ctx).time_base };
                self.mux_packet(self.a_pkt, tb, false, false, out)?;
                unsafe {
                    av_packet_unref(self.a_pkt);
                }
            }
        }
        Ok(())
    }

    fn mux_packet(
        &mut self,
        pkt: *const AVPacket,
        tb: AVRational,
        is_video: bool,
        is_key: bool,
        out: &mut MuxerOutput,
    ) -> Result<(), LiveMuxerError> {
        // Rotate HLS segment cleanly on keyframe boundary after at least 1.0s of video.
        if is_video
            && is_key
            && self.hls_cur_video_frames >= self.options.fps as u64
            && !self.hls_cur_buf.is_empty()
        {
            let seq = self.hls_next_seq;
            self.hls_next_seq += 1;
            let dur = (self.hls_cur_video_frames as f64) / (self.options.fps as f64);
            let completed = std::mem::take(&mut self.hls_cur_buf);
            self.hls_segments.push_back((seq, dur, completed));
            while self.hls_segments.len() > HLS_MAX_SEGMENTS {
                self.hls_segments.pop_front();
            }
            self.hls_cur_video_frames = 0;
            self.hls_playlist = Self::render_playlist(&self.hls_segments, self.options.fps);
        }

        let ts_bytes = self.ts_muxer.write_packet(pkt, tb, is_video)?;
        if !ts_bytes.is_empty() {
            if is_video && is_key {
                self.mpegts_gop_buf.clear();
                out.mpegts_is_keyframe = true;
            }
            self.mpegts_gop_buf.extend_from_slice(&ts_bytes);
            self.hls_cur_buf.extend_from_slice(&ts_bytes);
            out.mpegts_chunks.push(ts_bytes);
        }
        if is_video {
            self.hls_cur_video_frames += 1;
        }

        let mp4_bytes = self.mp4_muxer.write_packet(pkt, tb, is_video)?;
        if !mp4_bytes.is_empty() {
            self.latest_mp4_fragment = Some(mp4_bytes.clone());
            match &mut out.mp4_fragment {
                Some(existing) => existing.extend_from_slice(&mp4_bytes),
                None => out.mp4_fragment = Some(mp4_bytes),
            }
        }

        Ok(())
    }

    fn upload_video_frame(&mut self, src: &VideoFrame) -> Result<(), LiveMuxerError> {
        let w = self.options.width as usize;
        let h = self.options.height as usize;
        let dst_frame = if self.v_using_vaapi {
            self.v_sw_frame
        } else {
            self.v_frame
        };

        if unsafe { av_frame_make_writable(dst_frame) } < 0 {
            return Err(LiveMuxerError::Encode("video staging frame not writable".into()));
        }

        let src_w = (src.width as usize).min(w);
        let src_h = (src.height as usize).min(h);
        let src_y_stride = src.linesize[0] as usize;
        let src_uv_stride = src.linesize[1] as usize;

        unsafe {
            let df = &mut *dst_frame;
            let dst_y_stride = df.linesize[0] as usize;
            let y_slice = std::slice::from_raw_parts_mut(df.data[0], dst_y_stride * h);
            let src_y = std::slice::from_raw_parts(src.data[0] as *const u8, src_y_stride * src.height as usize);
            for row in 0..src_h {
                let s_off = row * src_y_stride;
                let d_off = row * dst_y_stride;
                y_slice[d_off..d_off + src_w].copy_from_slice(&src_y[s_off..s_off + src_w]);
            }

            if self.v_using_vaapi {
                // Target is NV12
                let dst_uv_stride = df.linesize[1] as usize;
                let uv_slice = std::slice::from_raw_parts_mut(df.data[1], dst_uv_stride * (h / 2));
                if src.format == VideoFormat::NV12 && !src.data[1].is_null() {
                    let src_uv = std::slice::from_raw_parts(
                        src.data[1] as *const u8,
                        src_uv_stride * (src.height as usize / 2),
                    );
                    for row in 0..(src_h / 2) {
                        let s_off = row * src_uv_stride;
                        let d_off = row * dst_uv_stride;
                        uv_slice[d_off..d_off + src_w]
                            .copy_from_slice(&src_uv[s_off..s_off + src_w]);
                    }
                }
            } else {
                // Target is YUV420P
                let dst_u_stride = df.linesize[1] as usize;
                let dst_v_stride = df.linesize[2] as usize;
                let u_plane = std::slice::from_raw_parts_mut(df.data[1], dst_u_stride * (h / 2));
                let v_plane = std::slice::from_raw_parts_mut(df.data[2], dst_v_stride * (h / 2));

                if src.format == VideoFormat::NV12 && !src.data[1].is_null() {
                    let src_uv = std::slice::from_raw_parts(
                        src.data[1] as *const u8,
                        src_uv_stride * (src.height as usize / 2),
                    );
                    for row in 0..(src_h / 2) {
                        let s_off = row * src_uv_stride;
                        let u_off = row * dst_u_stride;
                        let v_off = row * dst_v_stride;
                        for col in 0..(src_w / 2) {
                            u_plane[u_off + col] = src_uv[s_off + col * 2];
                            v_plane[v_off + col] = src_uv[s_off + col * 2 + 1];
                        }
                    }
                } else if src.format == VideoFormat::I420
                    && !src.data[1].is_null()
                    && !src.data[2].is_null()
                {
                    let src_u = std::slice::from_raw_parts(
                        src.data[1] as *const u8,
                        src_uv_stride * (src.height as usize / 2),
                    );
                    let src_v_stride = src.linesize[2] as usize;
                    let src_v = std::slice::from_raw_parts(
                        src.data[2] as *const u8,
                        src_v_stride * (src.height as usize / 2),
                    );
                    for row in 0..(src_h / 2) {
                        u_plane[row * dst_u_stride..row * dst_u_stride + src_w / 2]
                            .copy_from_slice(&src_u[row * src_uv_stride..row * src_uv_stride + src_w / 2]);
                        v_plane[row * dst_v_stride..row * dst_v_stride + src_w / 2]
                            .copy_from_slice(&src_v[row * src_v_stride..row * src_v_stride + src_w / 2]);
                    }
                }
            }
        }

        if self.v_using_vaapi {
            unsafe {
                av_frame_unref(self.v_frame);
                if av_hwframe_get_buffer((*self.v_ctx).hw_frames_ctx, self.v_frame, 0) < 0 {
                    return Err(LiveMuxerError::Encode(
                        "av_hwframe_get_buffer failed".into(),
                    ));
                }
                if av_hwframe_transfer_data(self.v_frame, self.v_sw_frame, 0) < 0 {
                    return Err(LiveMuxerError::Encode(
                        "av_hwframe_transfer_data failed".into(),
                    ));
                }
                (*self.v_frame).width = self.options.width as i32;
                (*self.v_frame).height = self.options.height as i32;
            }
        }

        Ok(())
    }

    fn render_playlist(segments: &VecDeque<(u64, f64, Vec<u8>)>, _fps: u8) -> String {
        let media_seq = segments.front().map(|(s, _, _)| *s).unwrap_or(0);
        let target_dur = segments
            .iter()
            .map(|(_, d, _)| d.ceil() as u64)
            .max()
            .unwrap_or(2)
            .max(1);
        let mut s = format!(
            "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n",
            target_dur, media_seq
        );
        for (seq, dur, _) in segments {
            s.push_str(&format!("#EXTINF:{:.3},\nseg_{}.ts\n", dur, seq));
        }
        s
    }
}

impl Drop for UniversalLiveMuxer {
    fn drop(&mut self) {
        unsafe {
            if !self.v_pkt.is_null() {
                av_packet_free(&mut self.v_pkt);
            }
            if !self.a_pkt.is_null() {
                av_packet_free(&mut self.a_pkt);
            }
            if !self.v_sw_frame.is_null() {
                av_frame_free(&mut self.v_sw_frame);
            }
            if !self.v_frame.is_null() {
                av_frame_free(&mut self.v_frame);
            }
            if !self.a_frame.is_null() {
                av_frame_free(&mut self.a_frame);
            }
            if !self.v_ctx.is_null() {
                if !(*self.v_ctx).hw_device_ctx.is_null() {
                    av_buffer_unref(&mut (*self.v_ctx).hw_device_ctx);
                }
                if !(*self.v_ctx).hw_frames_ctx.is_null() {
                    av_buffer_unref(&mut (*self.v_ctx).hw_frames_ctx);
                }
                avcodec_free_context(&mut self.v_ctx);
            }
            if !self.a_ctx.is_null() {
                avcodec_free_context(&mut self.a_ctx);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn muxer_produces_ts_hls_and_fmp4() {
        let mut muxer = UniversalLiveMuxer::new(LiveMuxerOptions {
            width: 320,
            height: 240,
            fps: 30,
            video_bitrate: 1_000_000,
            audio_sample_rate: 48_000,
            audio_bitrate: 128_000,
            encoder_type: VideoEncoderType::X264,
        })
        .expect("init UniversalLiveMuxer");

        let init = muxer.fmp4_init_segment();
        assert!(init.windows(4).any(|w| w == b"ftyp"), "missing ftyp in fMP4 init");
        assert!(init.windows(4).any(|w| w == b"moov"), "missing moov in fMP4 init");

        let y = vec![90u8; 320 * 240];
        let uv = vec![128u8; 320 * 120];
        let vf = VideoFrame {
            format: VideoFormat::NV12,
            sub_format: VideoSubFormat::SW,
            width: 320,
            height: 240,
            data: [
                y.as_ptr() as *const c_void,
                uv.as_ptr() as *const c_void,
                std::ptr::null(),
            ],
            linesize: [320, 320, 0],
        };

        let pcm = vec![120i16; 1024 * 2];
        let af = AudioFrame {
            sample_rate: 48_000,
            frames: 1024,
            data: pcm.as_ptr(),
        };
        let _ = muxer.push_audio(&af).expect("push audio");

        let mut total_ts = Vec::new();
        let mut saw_mp4_frag = false;
        for _ in 0..48 {
            let out = muxer.push_video(&vf).expect("push video");
            for c in out.mpegts_chunks {
                total_ts.extend_from_slice(&c);
            }
            if let Some(frag) = out.mp4_fragment {
                if frag.windows(4).any(|w| w == b"moof") {
                    saw_mp4_frag = true;
                }
            }
        }

        assert!(!total_ts.is_empty(), "expected non-empty MPEG-TS output");
        assert_eq!(total_ts[0], 0x47, "MPEG-TS must start with 0x47 sync byte");
        assert!(saw_mp4_frag, "expected fragmented MP4 moof chunk after GOP boundary");

        let playlist = muxer.hls_playlist();
        assert!(playlist.starts_with("#EXTM3U"));
        assert!(playlist.contains("seg_0.ts"), "playlist: {}", playlist);
        let seg0 = muxer.hls_segment("seg_0.ts").expect("seg_0.ts present");
        assert_eq!(seg0[0], 0x47);
    }
}
