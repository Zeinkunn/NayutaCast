package com.nayutacast.app

import android.content.Context
import android.net.wifi.WifiManager
import android.util.Log
import com.nayutacast.Discovery
import com.nayutacast.DiscoveryService
import com.nayutacast.DiscoveryServiceObserver
import com.nayutacast.MediaStreamDescription
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.util.concurrent.atomic.AtomicBoolean

class DeviceManager(
    context: Context,
    private val settings: Settings,
    private val scope: CoroutineScope,
    private val onP2pReadyChanged: (Boolean) -> Unit,
    private val onStreamStart: (peerId: String, senderName: String, senderIp: String, description: MediaStreamDescription) -> Unit,
    private val onStreamStop: (peerId: String) -> Unit,
    private val onPeerOffline: (peerId: String) -> Unit,
) {
    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }
    private val released = AtomicBoolean(false)
    private var multicastLock: WifiManager.MulticastLock? = null
    @Volatile
    private var discovery: DiscoveryService? = null

    init {
        val wifi = context.applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
        runCatching {
            multicastLock = wifi?.createMulticastLock("nayutacast-tv-discovery")?.apply {
                setReferenceCounted(false)
                acquire()
            }
        }.onFailure {
            Log.w("nayutacast", "Unable to acquire Wifi MulticastLock: ${it.message}")
        }

        scope.launch(Dispatchers.IO) {
            startDiscoveryService()
        }
    }

    private fun startDiscoveryService() {
        if (released.get() || discovery != null) {
            return
        }

        val service = runCatching {
            Discovery().createService("nayutacast-app-core", object : DiscoveryServiceObserver() {
                override fun onLine(localId: String, id: String, ip: String) {
                    Log.i("nayutacast", "P2P peer online: localId=$localId id=$id ip=$ip")
                    scope.launch(Dispatchers.IO) {
                        broadcastIdentity(listOf(id))
                    }
                }

                override fun offLine(localId: String, id: String, ip: String) {
                    Log.i("nayutacast", "P2P peer offline: localId=$localId id=$id")
                    onPeerOffline(id)
                }

                override fun onMessage(localId: String, id: String, ip: String, message: ByteArray) {
                    try {
                        val raw = String(message, Charsets.UTF_8)
                        val msg = json.decodeFromString<ServiceMessage>(raw)
                        if (msg.targets.isNotEmpty() && !msg.targets.contains(localId)) {
                            return
                        }

                        val desc = msg.description
                        if (desc != null) {
                            onStreamStart(
                                id,
                                msg.name.ifBlank { ip },
                                ip,
                                desc,
                            )
                        } else if (msg.kind != DeviceType.Android || msg.targets.contains(localId)) {
                            onStreamStop(id)
                        }
                    } catch (e: Exception) {
                        Log.e("nayutacast", "Failed to decode ServiceMessage", e)
                    }
                }
            })
        }.onFailure {
            Log.w("nayutacast", "libp2p Discovery unavailable (LAN HTTP 60344 remains active): ${it.message}")
        }.getOrNull()

        if (released.get()) {
            runCatching { service?.release() }
            return
        }

        discovery = service
        onP2pReadyChanged(service != null)
        if (service != null) {
            broadcastIdentity()
        }
    }

    fun broadcastIdentity(targets: List<String> = emptyList()) {
        val current = discovery ?: return
        val payload = ServiceMessage(
            targets = targets,
            name = settings.value.system.name,
            kind = DeviceType.Android,
            description = null,
        )
        runCatching {
            current.broadcast(json.encodeToString(payload).toByteArray(Charsets.UTF_8))
        }.onFailure {
            Log.w("nayutacast", "Failed to broadcast identity: ${it.message}")
        }
    }

    fun refresh() {
        scope.launch(Dispatchers.IO) {
            if (discovery == null) {
                startDiscoveryService()
            } else {
                broadcastIdentity()
            }
        }
    }

    fun release() {
        if (!released.compareAndSet(false, true)) {
            return
        }
        runCatching { discovery?.release() }
        discovery = null
        runCatching {
            if (multicastLock?.isHeld == true) {
                multicastLock?.release()
            }
        }
        multicastLock = null
    }

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
