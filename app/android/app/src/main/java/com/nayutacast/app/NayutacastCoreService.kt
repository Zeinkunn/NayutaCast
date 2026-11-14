package com.nayutacast.app

import android.app.Service
import android.content.Intent
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioTrack
import android.os.Binder
import android.os.IBinder
import android.util.Log
import android.view.Surface
import com.nayutacast.Audio
import com.nayutacast.MediaStreamDescription
import com.nayutacast.NayutacastReceiver
import com.nayutacast.NayutacastReceiverObserver
import com.nayutacast.NayutacastService

class NayutacastCoreService : Service() {
    @Volatile
    private var receiver: NayutacastReceiver? = null
    private val receiverLock = Any()

    override fun onBind(intent: Intent): IBinder {
        return ServiceBinder(this)
    }

    override fun onDestroy() {
        stopReceiver()
        super.onDestroy()
    }

    fun createReceiver(
        surface: Surface,
        description: MediaStreamDescription,
        observer: Observer
    ): Boolean {
        stopReceiver()

        return try {
            val audioTrack = createLowLatencyAudioTrack()
            val created = NayutacastService.createReceiver(
                description,
                object : NayutacastReceiverObserver() {
                    override val surface: Surface = surface
                    override val track: AudioTrack? = audioTrack

                    override fun onVideoSizeChanged(width: Int, height: Int) {
                        observer.onVideoSizeChanged(width, height)
                    }

                    override fun close() {
                        synchronized(receiverLock) {
                            runCatching { receiver?.release() }
                            receiver = null
                        }
                        observer.onClosed()
                    }
                }
            )
            synchronized(receiverLock) {
                receiver = created
            }
            true
        } catch (e: Exception) {
            Log.e("nayutacast", "Failed to create SRT receiver", e)
            false
        }
    }

    fun stopReceiver() {
        synchronized(receiverLock) {
            runCatching { receiver?.release() }
            receiver = null
        }
    }

    private fun createLowLatencyAudioTrack(): AudioTrack? {
        val audioConfig = Audio.getAudioCodecConfigure()
        val channelMask = AudioFormat.CHANNEL_OUT_STEREO
        val minBuf = AudioTrack.getMinBufferSize(
            audioConfig.sampleRate,
            channelMask,
            audioConfig.sampleBits
        ).coerceAtLeast(audioConfig.sampleRate / 10 * 4)
        val bufferSize = maxOf(minBuf, audioConfig.sampleRate / 10 * 4)

        val attrs = AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_MOVIE)
            .build()
        val format = AudioFormat.Builder()
            .setEncoding(audioConfig.sampleBits)
            .setSampleRate(audioConfig.sampleRate)
            .setChannelMask(channelMask)
            .build()

        return runCatching {
            AudioTrack.Builder()
                .setAudioAttributes(attrs)
                .setAudioFormat(format)
                .setPerformanceMode(AudioTrack.PERFORMANCE_MODE_LOW_LATENCY)
                .setTransferMode(AudioTrack.MODE_STREAM)
                .setBufferSizeInBytes(bufferSize)
                .build()
        }.getOrElse {
            runCatching {
                AudioTrack.Builder()
                    .setAudioAttributes(attrs)
                    .setAudioFormat(format)
                    .setTransferMode(AudioTrack.MODE_STREAM)
                    .setBufferSizeInBytes(bufferSize)
                    .build()
            }.onFailure { err ->
                Log.w("nayutacast", "AudioTrack unavailable on device: ${err.message}")
            }.getOrNull()
        }
    }

    class ServiceBinder(private val service: NayutacastCoreService) : Binder() {
        fun createReceiver(
            surface: Surface,
            description: MediaStreamDescription,
            observer: Observer
        ): Boolean {
            return service.createReceiver(surface, description, observer)
        }

        fun stopReceiver() {
            service.stopReceiver()
        }
    }

    abstract class Observer {
        open fun onVideoSizeChanged(width: Int, height: Int) {}
        abstract fun onClosed()
    }
}
