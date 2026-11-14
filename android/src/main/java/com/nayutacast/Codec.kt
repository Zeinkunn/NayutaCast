package com.nayutacast

import android.media.AudioFormat
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaCodec
import android.media.MediaCodecInfo
import android.media.MediaCodecList
import android.media.MediaFormat
import android.os.Build
import android.os.Process
import android.util.Log
import android.view.Surface
import java.nio.ByteBuffer
import java.util.concurrent.atomic.AtomicBoolean

abstract class ByteArraySinker {
    abstract fun sink(info: StreamBufferInfo, buf: ByteArray)
}

class Video {
    class VideoEncoder(
        configure: VideoEncoderConfigure,
        private val sinker: ByteArraySinker
    ) {
        private var isRunning: Boolean = false
        private var videoFormat: VideoFormat = VideoFormat.NV12

        private val codec: MediaCodec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
        private val bufferInfo = MediaCodec.BufferInfo()
        private var surface: Surface? = null
        private var worker: Thread

        init {
            val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, configure.width, configure.height)
            format.setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_VBR)
            format.setInteger(MediaFormat.KEY_PROFILE, MediaCodecInfo.CodecProfileLevel.AVCProfileBaseline)
            format.setLong(MediaFormat.KEY_REPEAT_PREVIOUS_FRAME_AFTER, 1_000_000 / configure.frameRate.toLong())
            format.setInteger(MediaFormat.KEY_COLOR_RANGE, MediaFormat.COLOR_RANGE_LIMITED)
            format.setFloat(MediaFormat.KEY_MAX_FPS_TO_ENCODER, configure.frameRate.toFloat())
            format.setInteger(MediaFormat.KEY_OPERATING_RATE, configure.frameRate)
            format.setInteger(MediaFormat.KEY_CAPTURE_RATE, configure.frameRate)
            format.setInteger(MediaFormat.KEY_FRAME_RATE, configure.frameRate)
            format.setInteger(MediaFormat.KEY_COLOR_FORMAT, configure.format)
            format.setInteger(MediaFormat.KEY_BIT_RATE, configure.bitRate)
            format.setFloat(MediaFormat.KEY_I_FRAME_INTERVAL, 0.4F)
            format.setInteger(MediaFormat.KEY_MAX_B_FRAMES, 0)
            format.setInteger(MediaFormat.KEY_LATENCY, 2)
            format.setInteger(
                MediaFormat.KEY_LEVEL, if (configure.width <= 1280 && configure.height <= 720) {
                    MediaCodecInfo.CodecProfileLevel.AVCLevel31
                } else if (configure.width <= 2048 && configure.height <= 1024) {
                    MediaCodecInfo.CodecProfileLevel.AVCLevel4
                } else {
                    MediaCodecInfo.CodecProfileLevel.AVCLevel5
                }
            )

            if (codec.name.indexOf(".qti.") >= 0) {
                videoFormat = VideoFormat.I420
            }

            if (codec.name.indexOf(".rk.") >= 0) {
                format.setInteger(MediaFormat.KEY_COMPLEXITY, 0)
                format.setInteger(MediaFormat.KEY_PRIORITY, 0)
            }

            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                format.setInteger(MediaFormat.KEY_ALLOW_FRAME_DROP, 1)
            }

            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)
            surface =
                if (configure.format == MediaCodecInfo.CodecCapabilities.COLOR_FormatSurface) {
                    codec.createInputSurface()
                } else {
                    null
                }

            worker = Thread {
                Process.setThreadPriority(Process.THREAD_PRIORITY_VIDEO)

                val buffer = ByteArray(2 * 1024 * 1024)
                val streamBufferInfo = StreamBufferInfo(StreamType.VIDEO.flag)

                while (isRunning) {
                    try {
                        val index = codec.dequeueOutputBuffer(bufferInfo, -1)
                        if (index >= 0) {
                            val outputBuffer = codec.getOutputBuffer(index)
                            if (outputBuffer != null && bufferInfo.size > 0) {
                                streamBufferInfo.flags = bufferInfo.flags
                                streamBufferInfo.timestamp = bufferInfo.presentationTimeUs
                                outputBuffer.get(buffer, 0, bufferInfo.size)

                                sinker.sink(
                                    streamBufferInfo,
                                    buffer.sliceArray(bufferInfo.offset until bufferInfo.size),
                                )
                            }

                            codec.releaseOutputBuffer(index, false)
                        }
                    } catch (e: Exception) {
                        Log.w("com.nayutacast", "VideoEncoder worker exception", e)

                        release()
                    }
                }
            }
        }

        fun sink(buf: ByteArray) {
            val index = codec.dequeueInputBuffer(-1)
            if (index >= 0) {
                codec.getInputBuffer(index)?.clear()
                codec.getInputBuffer(index)?.put(buf)
                codec.queueInputBuffer(index, 0, buf.size, 0, 0)
            }
        }

        fun getSurface(): Surface? {
            return surface
        }

        fun getFormat(): VideoFormat {
            return videoFormat
        }

        fun start() {
            if (!isRunning) {
                isRunning = true

                codec.start()
                worker.start()
            }
        }

        fun release() {
            if (isRunning) {
                isRunning = false

                codec.stop()
                codec.release()
            }
        }

        data class VideoEncoderConfigure(

            /**
             * [MediaCodecInfo.CodecCapabilities](https://developer.android.com/reference/android/media/MediaCodecInfo.CodecCapabilities)
             */
            val format: Int,
            var width: Int,
            var height: Int,

            /**
             * [MediaFormat#KEY_BIT_RATE](https://developer.android.com/reference/android/media/MediaFormat#KEY_BIT_RATE)
             */
            val bitRate: Int,

            /**
             * [MediaFormat#KEY_FRAME_RATE](https://developer.android.com/reference/android/media/MediaFormat#KEY_FRAME_RATE)
             */
            val frameRate: Int,
        )
    }

    class VideoDecoder(
        surface: Surface,
        width: Int,
        height: Int,
        private val onVideoSizeChanged: ((Int, Int) -> Unit)? = null
    ) {
        @Volatile
        var isRunning: Boolean = false
            private set

        private val released = AtomicBoolean(false)
        private val codec: MediaCodec
        private val bufferInfo = MediaCodec.BufferInfo()
        private val worker: Thread

        init {
            codec = createAndConfigureDecoder(surface, width.coerceAtLeast(16), height.coerceAtLeast(16))
            worker = Thread({
                Process.setThreadPriority(Process.THREAD_PRIORITY_VIDEO)

                while (isRunning) {
                    try {
                        val index = codec.dequeueOutputBuffer(bufferInfo, 10_000L)
                        when {
                            index >= 0 -> {
                                codec.releaseOutputBuffer(index, bufferInfo.size > 0)
                            }
                            index == MediaCodec.INFO_OUTPUT_FORMAT_CHANGED -> {
                                val outFmt = codec.outputFormat
                                val outWidth =
                                    if (outFmt.containsKey(MediaFormat.KEY_WIDTH)) {
                                        outFmt.getInteger(MediaFormat.KEY_WIDTH)
                                    } else {
                                        0
                                    }
                                val outHeight =
                                    if (outFmt.containsKey(MediaFormat.KEY_HEIGHT)) {
                                        outFmt.getInteger(MediaFormat.KEY_HEIGHT)
                                    } else {
                                        0
                                    }
                                if (outWidth > 0 && outHeight > 0) {
                                    onVideoSizeChanged?.invoke(outWidth, outHeight)
                                }
                            }
                        }
                    } catch (e: Exception) {
                        if (isRunning) {
                            Log.w("com.nayutacast", "VideoDecoder worker exception", e)
                        }
                        isRunning = false
                        break
                    }
                }
            }, "NayutaCast-VideoDecoder")
        }

        private fun createAndConfigureDecoder(
            surface: Surface,
            width: Int,
            height: Int
        ): MediaCodec {
            var lowLatencyCodecName: String? = null
            val codecList = MediaCodecList(MediaCodecList.REGULAR_CODECS)
            for (codecInfo in codecList.codecInfos) {
                if (!codecInfo.isEncoder && codecInfo.isHardwareAccelerated) {
                    if (codecInfo.supportedTypes.any { it.equals(MediaFormat.MIMETYPE_VIDEO_AVC, ignoreCase = true) } &&
                        codecInfo.name.contains("low_latency", ignoreCase = true)
                    ) {
                        lowLatencyCodecName = codecInfo.name
                        break
                    }
                }
            }

            fun buildFormat(targetCodec: MediaCodec): MediaFormat {
                val format = MediaFormat.createVideoFormat(MediaFormat.MIMETYPE_VIDEO_AVC, width, height)
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.R) {
                    if (!targetCodec.name.contains(".rk.") && !targetCodec.name.contains(".hisi.")) {
                        val supported = runCatching {
                            targetCodec.codecInfo
                                .getCapabilitiesForType(MediaFormat.MIMETYPE_VIDEO_AVC)
                                .isFeatureSupported(MediaCodecInfo.CodecCapabilities.FEATURE_LowLatency)
                        }.getOrDefault(false)
                        if (supported) {
                            format.setInteger(MediaFormat.KEY_LOW_LATENCY, 1)
                        }
                    }
                }
                return format
            }

            if (lowLatencyCodecName != null) {
                val preferred = runCatching {
                    val candidate = MediaCodec.createByCodecName(lowLatencyCodecName)
                    try {
                        candidate.configure(buildFormat(candidate), surface, null, 0)
                        candidate
                    } catch (e: Exception) {
                        runCatching { candidate.release() }
                        throw e
                    }
                }.getOrNull()
                if (preferred != null) {
                    return preferred
                }
            }

            val fallback = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_VIDEO_AVC)
            try {
                fallback.configure(buildFormat(fallback), surface, null, 0)
            } catch (e: Exception) {
                runCatching { fallback.release() }
                throw e
            }
            return fallback
        }

        fun sink(buf: ByteArray, flags: Int, timestamp: Long) {
            if (!isRunning || buf.isEmpty()) {
                return
            }

            try {
                val index = codec.dequeueInputBuffer(20_000L)
                if (index >= 0) {
                    val inputBuffer = codec.getInputBuffer(index)
                    if (inputBuffer == null) {
                        codec.queueInputBuffer(index, 0, 0, 0L, 0)
                        return
                    }
                    inputBuffer.clear()
                    if (buf.size > inputBuffer.remaining()) {
                        Log.w(
                            "com.nayutacast",
                            "VideoDecoder packet (${buf.size}B) exceeds input buffer (${inputBuffer.remaining()}B)"
                        )
                        codec.queueInputBuffer(index, 0, 0, 0L, 0)
                        return
                    }
                    inputBuffer.put(buf)
                    val safeFlags = flags and (
                        MediaCodec.BUFFER_FLAG_KEY_FRAME or
                            MediaCodec.BUFFER_FLAG_CODEC_CONFIG or
                            MediaCodec.BUFFER_FLAG_END_OF_STREAM
                        )
                    codec.queueInputBuffer(index, 0, buf.size, timestamp.coerceAtLeast(0L), safeFlags)
                }
            } catch (e: Exception) {
                if (isRunning) {
                    Log.w("com.nayutacast", "VideoDecoder sink exception", e)
                }
            }
        }

        fun start() {
            if (!released.get() && !isRunning) {
                isRunning = true

                codec.start()
                worker.start()
            }
        }

        fun release() {
            if (!released.compareAndSet(false, true)) {
                return
            }

            val wasRunning = isRunning
            isRunning = false
            if (wasRunning && Thread.currentThread() !== worker) {
                runCatching { worker.join(500L) }
            }
            if (wasRunning) {
                runCatching { codec.stop() }
            }
            runCatching { codec.release() }
        }
    }
}

class Audio {
    data class AudioCodecConfigure(

        /**
         * [AudioFormat#ENCODING_PCM_16BIT](https://developer.android.com/reference/android/media/AudioFormat#ENCODING_PCM_16BIT)
         */
        val sampleBits: Int,

        /**
         * [AudioFormat#SAMPLE_RATE_UNSPECIFIED](https://developer.android.com/reference/android/media/AudioFormat#SAMPLE_RATE_UNSPECIFIED)
         */
        val sampleRate: Int,

        /**
         * Number of audio channels, such as mono or stereo (dual channel)
         */
        val channels: Int,

        /**
         * [MediaFormat#KEY_BIT_RATE](https://developer.android.com/reference/android/media/MediaFormat#KEY_BIT_RATE)
         */
        val bitRate: Int,
    )

    companion object {
        fun getAudioCodecConfigure(): AudioCodecConfigure {
            return AudioCodecConfigure(
                sampleBits = AudioFormat.ENCODING_PCM_16BIT,
                sampleRate = 48000,
                bitRate = 64000,
                channels = 2,
            )
        }
    }

    class AudioDecoder(private val track: AudioTrack) {
        @Volatile
        var isRunning: Boolean = false
            private set

        private val released = AtomicBoolean(false)
        private val bufferInfo = MediaCodec.BufferInfo()
        private val codec: MediaCodec
        private val worker: Thread

        init {
            val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, 48000, 2)
            format.setInteger(MediaFormat.KEY_PCM_ENCODING, AudioFormat.ENCODING_PCM_16BIT)

            codec = MediaCodec.createDecoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
            try {
                codec.configure(format, null, null, 0)
            } catch (e: Exception) {
                runCatching { codec.release() }
                throw e
            }

            worker = Thread({
                Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)

                var buf = ByteArray(1024 * 64)

                while (isRunning) {
                    try {
                        val index = codec.dequeueOutputBuffer(bufferInfo, 10_000L)
                        if (index >= 0) {
                            val outputBuffer = codec.getOutputBuffer(index)
                            if (outputBuffer != null && bufferInfo.size > 0) {
                                if (bufferInfo.size > buf.size) {
                                    buf = ByteArray(bufferInfo.size)
                                }
                                outputBuffer.position(bufferInfo.offset)
                                outputBuffer.limit(bufferInfo.offset + bufferInfo.size)
                                outputBuffer.get(buf, 0, bufferInfo.size)
                                track.write(buf, 0, bufferInfo.size)
                            }

                            codec.releaseOutputBuffer(index, false)
                        }
                    } catch (e: Exception) {
                        if (isRunning) {
                            Log.w("com.nayutacast", "AudioDecoder worker exception", e)
                        }
                        isRunning = false
                        break
                    }
                }
            }, "NayutaCast-AudioDecoder")
        }

        fun sink(buf: ByteArray, flags: Int, timestamp: Long) {
            if (!isRunning || buf.isEmpty()) {
                return
            }

            try {
                val index = codec.dequeueInputBuffer(10_000L)
                if (index >= 0) {
                    val inputBuffer = codec.getInputBuffer(index)
                    if (inputBuffer == null) {
                        codec.queueInputBuffer(index, 0, 0, 0L, 0)
                        return
                    }
                    inputBuffer.clear()
                    if (buf.size > inputBuffer.remaining()) {
                        codec.queueInputBuffer(index, 0, 0, 0L, 0)
                        return
                    }
                    inputBuffer.put(buf)
                    val safeFlags = flags and (
                        MediaCodec.BUFFER_FLAG_KEY_FRAME or
                            MediaCodec.BUFFER_FLAG_CODEC_CONFIG or
                            MediaCodec.BUFFER_FLAG_END_OF_STREAM
                        )
                    codec.queueInputBuffer(index, 0, buf.size, timestamp.coerceAtLeast(0L), safeFlags)
                }
            } catch (e: Exception) {
                if (isRunning) {
                    Log.w("com.nayutacast", "AudioDecoder sink exception", e)
                }
            }
        }

        fun start() {
            if (!released.get() && !isRunning) {
                isRunning = true

                codec.start()
                worker.start()
                track.play()
            }
        }

        fun release() {
            if (!released.compareAndSet(false, true)) {
                return
            }

            val wasRunning = isRunning
            isRunning = false
            if (wasRunning && Thread.currentThread() !== worker) {
                runCatching { worker.join(500L) }
            }
            if (wasRunning) {
                runCatching { track.stop() }
                runCatching { codec.stop() }
            }
            runCatching { track.release() }
            runCatching { codec.release() }
        }
    }

    class AudioEncoder(
        private val record: AudioRecord?,
        private val sinker: ByteArraySinker
    ) {
        private var isRunning: Boolean = false

        private val bufferInfo = MediaCodec.BufferInfo()
        private var codec: MediaCodec
        private var worker: Thread
        private var recorder: Thread? = null

        private val minBufferSize = AudioRecord.getMinBufferSize(
            48000,
            AudioFormat.CHANNEL_IN_STEREO,
            AudioFormat.ENCODING_PCM_16BIT
        )

        init {
            val format = MediaFormat.createAudioFormat(MediaFormat.MIMETYPE_AUDIO_OPUS, 48000, 2)
            format.setInteger(MediaFormat.KEY_BITRATE_MODE, MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR)
            format.setInteger(MediaFormat.KEY_PCM_ENCODING, AudioFormat.ENCODING_PCM_16BIT)
            format.setInteger(MediaFormat.KEY_MAX_INPUT_SIZE, minBufferSize);
            format.setInteger(MediaFormat.KEY_CHANNEL_COUNT, 2)
            format.setInteger(MediaFormat.KEY_BIT_RATE, 64000)
            format.setLong(MediaFormat.KEY_DURATION, 1000 * 20)
            format.setInteger(MediaFormat.KEY_COMPLEXITY, 0)

            codec = MediaCodec.createEncoderByType(MediaFormat.MIMETYPE_AUDIO_OPUS)
            codec.configure(format, null, null, MediaCodec.CONFIGURE_FLAG_ENCODE)

            worker = Thread {
                Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)

                val buffer = ByteArray(1024 * 1024)
                val streamBufferInfo = StreamBufferInfo(StreamType.AUDIO.flag)

                while (isRunning) {
                    try {
                        val index = codec.dequeueOutputBuffer(bufferInfo, -1)
                        if (index >= 0) {
                            val outputBuffer = codec.getOutputBuffer(index)
                            if (outputBuffer != null && bufferInfo.size > 0) {
                                streamBufferInfo.flags = bufferInfo.flags
                                streamBufferInfo.timestamp = bufferInfo.presentationTimeUs
                                outputBuffer.get(buffer, 0, bufferInfo.size)

                                sinker.sink(
                                    streamBufferInfo,
                                    buffer.sliceArray(bufferInfo.offset until bufferInfo.size),
                                )
                            }

                            codec.releaseOutputBuffer(index, false)
                        }
                    } catch (e: Exception) {
                        Log.w("com.nayutacast", "AudioEncoder worker exception", e)

                        release()
                    }
                }
            }

            if (record != null) {
                recorder = Thread {
                    Process.setThreadPriority(Process.THREAD_PRIORITY_URGENT_AUDIO)

                    while (isRunning) {
                        try {
                            val buf = ByteBuffer.allocateDirect(minBufferSize)
                            val size = record.read(buf, buf.capacity(), AudioRecord.READ_BLOCKING)
                            if (size > 0) {
                                val index = codec.dequeueInputBuffer(-1)
                                if (index >= 0) {
                                    codec.getInputBuffer(index)?.put(buf)
                                    codec.queueInputBuffer(index, 0, size, 0, 0)
                                }
                            }
                        } catch (e: Exception) {
                            Log.w("com.nayutacast", "AudioDecoder record exception", e)

                            release()
                        }
                    }
                }
            }
        }

        fun sink(buf: ByteArray) {
            val index = codec.dequeueInputBuffer(-1)
            if (index >= 0) {
                codec.getInputBuffer(index)?.clear()
                codec.getInputBuffer(index)?.put(buf)
                codec.queueInputBuffer(index, 0, buf.size, 0, 0)
            }
        }

        fun start() {
            if (!isRunning) {
                isRunning = true

                codec.start()
                worker.start()
                recorder?.start()
                record?.startRecording()
            }
        }

        fun release() {
            if (isRunning) {
                isRunning = false

                record?.stop()
                codec.stop()
                codec.release()
            }
        }
    }
}