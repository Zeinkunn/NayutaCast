package com.nayutacast.app

import android.content.Context
import android.os.Build
import android.util.Log
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.io.File
import java.util.Locale

class Settings(context: Context) {
    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }
    private val file = File(context.filesDir, "settings.json")
    private var model: Model

    var value: Model
        get() = model
        set(value) {
            model = value
            raw = json.encodeToString(value)
        }

    var raw: String
        get() = file.readText()
        set(value) {
            file.writeText(value)
        }

    init {
        val defaultRaw = context.resources.openRawResource(R.raw.settings)
            .bufferedReader()
            .use { it.readText() }

        if (!file.exists()) {
            model = createDefaultModel(context, defaultRaw)
            raw = json.encodeToString(model)
        } else {
            val loaded = runCatching {
                json.decodeFromString<Model>(raw)
            }.getOrNull()

            if (loaded != null) {
                if (loaded.system.name.isBlank()) {
                    loaded.system.name = buildDefaultDeviceName()
                }
                model = loaded
            } else {
                Log.w("nayutacast", "Resetting unreadable settings.json to defaults")
                model = createDefaultModel(context, defaultRaw)
                raw = json.encodeToString(model)
            }
        }
    }

    fun updateDeviceName(newName: String) {
        val clean = newName.trim().ifEmpty { buildDefaultDeviceName() }
        value = model.copy(
            system = model.system.copy(name = clean)
        )
    }

    private fun createDefaultModel(context: Context, defaultRaw: String): Model {
        val settings = json.decodeFromString<Model>(defaultRaw)
        settings.system.name = buildDefaultDeviceName()
        settings.video.width = context.resources.displayMetrics.widthPixels.coerceAtLeast(1280)
        settings.video.height = context.resources.displayMetrics.heightPixels.coerceAtLeast(720)
        settings.video.frameRate = 30
        return settings
    }

    companion object {
        fun buildDefaultDeviceName(): String {
            val manufacturer = Build.MANUFACTURER?.trim().orEmpty()
            val model = Build.MODEL?.trim().orEmpty()
            if (manufacturer.isEmpty() && model.isEmpty()) {
                return "NayutaCast TV"
            }
            if (manufacturer.isEmpty()) {
                return model
            }
            if (model.isEmpty()) {
                return manufacturer.replaceFirstChar {
                    if (it.isLowerCase()) it.titlecase(Locale.getDefault()) else it.toString()
                } + " TV"
            }
            if (model.startsWith(manufacturer, ignoreCase = true)) {
                return model
            }
            val prettyBrand = manufacturer.replaceFirstChar {
                if (it.isLowerCase()) it.titlecase(Locale.getDefault()) else it.toString()
            }
            return "$prettyBrand $model"
        }
    }

    @Serializable
    data class Model(
        var network: Network,
        var system: System,
        var codec: Codec,
        var video: Video,
        var audio: Audio
    )

    @Serializable
    data class System(
        var name: String,
        var language: String,
    )

    @Serializable
    data class Network(
        @SerialName("interface")
        var inter: String,
        var multicast: String,
        var server: String?,
        var port: Int,
        var mtu: Int
    )

    @Serializable
    data class Codec(
        var encoder: String,
        var decoder: String
    )

    @Serializable
    data class Video(
        var width: Int,
        var height: Int,
        @SerialName("frame_rate")
        var frameRate: Int,
        @SerialName("bit_rate")
        var bitRate: Int,
        @SerialName("key_frame_interval")
        var keyFrameInterval: Int
    )

    @Serializable
    data class Audio(
        @SerialName("sample_rate")
        var sampleRate: Int,
        @SerialName("bit_rate")
        var bitRate: Int
    )
}
