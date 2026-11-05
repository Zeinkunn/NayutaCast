package com.nayutacast.app

import android.util.Log
import com.nayutacast.Discovery
import com.nayutacast.DiscoveryService
import com.nayutacast.DiscoveryServiceObserver
import com.nayutacast.MediaStreamDescription
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.launch
import kotlinx.serialization.Serializable
import kotlinx.serialization.json.Json

class DeviceManager(private var settings: Settings, scope: CoroutineScope) {
    private val channel = Channel<Unit>()
    private var discovery: DiscoveryService? = null
    private val devices: HashMap<String, Device> = HashMap()

    private var cacheTargets: List<String> = listOf()
    private var cacheDescription: MediaStreamDescription? = null

    val all: List<Device> get() = devices.values.toList()
    val watcher: Channel<Unit> get() = channel

    init {
        val onLineChannel = Channel<Unit>()

        discovery =
            Discovery().createService("nayutacast-app-core", object : DiscoveryServiceObserver() {
                override fun onLine(localId: String, id: String, ip: String) {
                    Log.i("nayutacast", "device manager on line, local_id=$localId, id=$id, ip=$ip")

                    scope.launch {
                        onLineChannel.send(Unit)
                    }
                }

                override fun offLine(localId: String, id: String, ip: String) {
                    Log.i("nayutacast", "device manager off line, local_id=$localId, id=$id")

                    devices.remove(id)

                    scope.launch {
                        channel.send(Unit)
                    }
                }

                override fun onMessage(localId: String, id: String, ip: String, message: ByteArray) {
                    Log.i("nayutacast", "device manager on message")

                    try {
                        val (targets, name, kind, description) = Json.decodeFromString<ServiceMessage>(
                            String(message, Charsets.UTF_8)
                        )

                        Log.i(
                            "nayutacast",
                            "device manager on message, targets=$targets, name=$name, kind=$kind, description=$description"
                        )

                        if (targets.isEmpty() || targets.contains(localId)) {
                            devices[id] = Device(name, ip, kind, description)

                            scope.launch {
                                channel.send(Unit)
                            }
                        }
                    } catch (e: Error) {
                        Log.e("nayutacast", "device manager on message error=$e")
                    }
                }
            })

        scope.launch {
            for (x in onLineChannel) {
                val payload = ServiceMessage(
                    cacheTargets,
                    settings.value.system.name,
                    kind = DeviceType.Android,
                    cacheDescription,
                )

                Log.i("nayutacast", "device manager send message=$payload")

                discovery?.broadcast(Json.encodeToString(payload).toByteArray(Charsets.UTF_8))
            }
        }
    }

    fun send(targets: List<String>, description: MediaStreamDescription?) {
        val payload = ServiceMessage(
            targets,
            settings.value.system.name,
            kind = DeviceType.Android,
            description
        )

        Log.i("nayutacast", "device manager send message=$payload")

        discovery?.broadcast(Json.encodeToString(payload).toByteArray(Charsets.UTF_8))

        cacheTargets = targets
        cacheDescription = description
    }

    fun release() {
        discovery?.release()
    }

    @Serializable
    data class Device(
        val name: String,
        val ip: String,
        val kind: DeviceType,
        val description: MediaStreamDescription?,
    )

    @Serializable
    enum class DeviceType {
        Windows,
        Android,
        Apple,
        Linux,
    }

    @Serializable
    data class ServiceMessage(
        val targets: List<String>,
        val name: String,
        val kind: DeviceType,
        val description: MediaStreamDescription?,
    )
}