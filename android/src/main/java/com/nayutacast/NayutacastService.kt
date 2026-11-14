package com.nayutacast

import android.media.AudioRecord
import android.media.AudioTrack
import android.util.Log
import android.view.Surface
import kotlin.Exception

data class NayutacastSenderConfigure(
    val video: Video.VideoEncoder.VideoEncoderConfigure,
    val options: TransportOptions,
)

abstract class NayutacastReceiverObserver {

    /**
     *  You need to provide a surface to the receiver, which will decode and render the received
     *  video stream to this surface.
     */
    abstract val surface: Surface

    /**
     * You need to provide an audio track to the receiver, which will decode the received audio
     * stream and play it using this audio track.
     */
    abstract val track: AudioTrack?

    /**
     * You can choose to implement this function, and the underlying transport layer will give you a c
     * opy of the audio and video data, with the `kind` parameter indicating the type of packet.
     */
    open fun sink(bytes: ByteArray, kind: StreamType) {}

    /**
     * Called when the video decoder reports the actual output video dimensions.
     */
    open fun onVideoSizeChanged(width: Int, height: Int) {}

    /**
     * Called when the receiver is closed, the likely reason is because the underlying transport
     * layer has been disconnected, perhaps because the sender has been closed or the network is
     * disconnected.
     */
    open fun close() {}
}

abstract class NayutacastSenderObserver {

    /**
     * A recorder that can record system sounds or other audio sources.
     */
    abstract val record: AudioRecord?

    /**
     * Called when the receiver is closed, the likely reason is because the underlying transport
     * layer has been disconnected, perhaps because the sender has been closed or the network is
     * disconnected.
     */
    open fun close() {}
}

/**
 * Create a nayutacast service, note that observer can be null, when observer is null, it will not
 * automatically respond to any sender push.
 */
class NayutacastService {
    companion object {
        private val nayutacast = Nayutacast()

        /**
         * Creates an instance of a sender with an unlimited `id` parameter, this id is passed to all
         * receivers and is mainly used to provide receivers with identification of this sender.
         */
        fun createSender(
            configure: NayutacastSenderConfigure,
            observer: NayutacastSenderObserver
        ): NayutacastSender {
            return NayutacastSender(
                observer,
                nayutacast.createSender(configure.options),
                configure,
                observer.record,
            )
        }

        /**
         * Creating a receiver and connecting to a specific sender results in a more proactive connection
         * than auto-discovery, and the handshake will take less time.
         *
         * `port` The port number from the created sender.
         */
        fun createReceiver(
            description: MediaStreamDescription,
            observer: NayutacastReceiverObserver
        ): NayutacastReceiver {
            return NayutacastReceiver(
                nayutacast.createReceiver(
                    description.id,
                    description.transport,
                    object : NayutacastReceiverAdapterObserver() {
                        @Volatile
                        private var isReleased = false
                        private val audioDecoder = observer.track?.let { track ->
                            runCatching {
                                Audio.AudioDecoder(track).apply { start() }
                            }.onFailure { err ->
                                Log.w(
                                    "com.nayutacast",
                                    "AudioDecoder unavailable, continuing with video only",
                                    err
                                )
                                runCatching { track.release() }
                            }.getOrNull()
                        }
                        private val videoDecoder = Video.VideoDecoder(
                            observer.surface,
                            description.video?.size?.width ?: 1920,
                            description.video?.size?.height ?: 1080
                        ) { width, height ->
                            observer.onVideoSizeChanged(width, height)
                        }

                        init {
                            try {
                                videoDecoder.start()
                            } catch (e: Exception) {
                                runCatching { audioDecoder?.release() }
                                runCatching { videoDecoder.release() }
                                throw e
                            }
                        }

                        override fun sink(
                            kind: Int,
                            flags: Int,
                            timestamp: Long,
                            bytes: ByteArray
                        ): Boolean {
                            try {
                                if (isReleased) {
                                    return false
                                }

                                if (bytes.isEmpty()) {
                                    return true
                                }

                                when (kind) {
                                    StreamType.VIDEO.flag -> {
                                        if (videoDecoder.isRunning) {
                                            videoDecoder.sink(bytes, flags, timestamp)
                                            observer.sink(bytes, StreamType.VIDEO)
                                        }
                                    }

                                    StreamType.AUDIO.flag -> {
                                        if (audioDecoder != null && audioDecoder.isRunning) {
                                            audioDecoder.sink(bytes, flags, timestamp)
                                            observer.sink(bytes, StreamType.AUDIO)
                                        }
                                    }
                                }

                                return true
                            } catch (e: Exception) {
                                Log.e(
                                    "com.nayutacast",
                                    "Nayutacast ReceiverAdapter sink exception",
                                    e
                                )

                                return false
                            }
                        }

                        override fun close() {
                            if (isReleased) {
                                return
                            }
                            isReleased = true

                            runCatching { audioDecoder?.release() }
                                .onFailure { Log.e("com.nayutacast", "AudioDecoder release failed", it) }
                            runCatching { videoDecoder.release() }
                                .onFailure { Log.e("com.nayutacast", "VideoDecoder release failed", it) }
                            runCatching { observer.close() }
                                .onFailure { Log.e("com.nayutacast", "Receiver observer close failed", it) }
                        }
                    }
                )
            )
        }
    }
}

class NayutacastReceiver(
    private val receiver: NayutacastReceiverAdapter
) {
    /**
     * Close and release this receiver.
     */
    fun release() {
        receiver.release()
    }
}

class NayutacastSender(
    private val observer: NayutacastSenderObserver,
    private val sender: NayutacastSenderAdapter,
    private val configure: NayutacastSenderConfigure,
    record: AudioRecord?,
) {
    private var isClosed = false
    private var isReleased = false

    private val videoEncoder: Video.VideoEncoder =
        Video.VideoEncoder(configure.video, object : ByteArraySinker() {
            override fun sink(info: StreamBufferInfo, buf: ByteArray) {
                if (!isClosed) {
                    if (!sender.send(info, buf)) {
                        isClosed = true
                        observer.close()
                    }
                }
            }
        })

    private val audioEncoder: Audio.AudioEncoder =
        Audio.AudioEncoder(record, object : ByteArraySinker() {
            override fun sink(info: StreamBufferInfo, buf: ByteArray) {
                if (!isClosed) {
                    if (!sender.send(info, buf)) {
                        isClosed = true
                        observer.close()
                    }
                }
            }
        })

    init {
        videoEncoder.start()
        audioEncoder.start()
    }

    /**
     * Get the surface inside the sender, you need to render the texture to this surface to pass the
     * screen to other receivers.
     */
    val surface: Surface? get() {
        return videoEncoder.getSurface()
    }

    /**
     * get sender stream id.
     */
    fun getDescription(): MediaStreamDescription {
        val audio = Audio.getAudioCodecConfigure()
        return MediaStreamDescription(
            sender.getId(),
            configure.options,
            MediaVideoStreamDescription(
                fps = configure.video.frameRate,
                bitRate = configure.video.bitRate,
                format = videoEncoder.getFormat().flag,
                size = Size(width = configure.video.width, height = configure.video.height),
            ),
            MediaAudioStreamDescription(
                sampleRate = audio.sampleRate,
                channels = audio.channels,
                bitRate = audio.bitRate
            ),
        )
    }

    /**
     * Close and release this sender.
     */
    fun release() {
        if (!isReleased) {
            isReleased = true

            audioEncoder.release()
            videoEncoder.release()
            sender.release()

            if (!isClosed) {
                isClosed = true
                observer.close()
            }
        }
    }
}