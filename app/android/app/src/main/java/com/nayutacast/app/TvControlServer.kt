package com.nayutacast.app

import android.os.Build
import android.util.Log
import com.nayutacast.MediaStreamDescription
import kotlinx.serialization.SerialName
import kotlinx.serialization.Serializable
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import java.io.ByteArrayOutputStream
import java.io.InputStream
import java.net.Inet4Address
import java.net.InetSocketAddress
import java.net.NetworkInterface
import java.net.ServerSocket
import java.net.Socket
import java.util.concurrent.atomic.AtomicBoolean

class TvControlServer(
    private val port: Int = CONTROL_PORT,
    private val appVersion: String,
    private val deviceNameProvider: () -> String,
    private val isCastingProvider: () -> Boolean,
    private val onStartStream: (senderIp: String, description: MediaStreamDescription) -> Unit,
    private val onStopStream: () -> Unit,
) {
    private val json = Json {
        ignoreUnknownKeys = true
        encodeDefaults = true
    }
    private val running = AtomicBoolean(false)
    @Volatile
    private var serverSocket: ServerSocket? = null
    @Volatile
    private var acceptThread: Thread? = null

    val isListening: Boolean
        get() = running.get() && serverSocket?.isBound == true && serverSocket?.isClosed == false

    fun start(): Boolean {
        if (!running.compareAndSet(false, true)) {
            return isListening
        }

        return try {
            val socket = ServerSocket()
            socket.reuseAddress = true
            socket.bind(InetSocketAddress("0.0.0.0", port))
            serverSocket = socket

            acceptThread = Thread({
                Log.i("nayutacast", "TvControlServer listening on 0.0.0.0:$port")
                while (running.get()) {
                    try {
                        val client = socket.accept()
                        Thread({
                            handleClient(client)
                        }, "NayutaCast-TvHttpWorker").apply {
                            isDaemon = true
                            start()
                        }
                    } catch (e: Exception) {
                        if (running.get()) {
                            Log.w("nayutacast", "TvControlServer accept error: ${e.message}")
                        }
                    }
                }
            }, "NayutaCast-TvHttpAccept").apply {
                isDaemon = true
                start()
            }
            true
        } catch (e: Exception) {
            Log.e("nayutacast", "Failed to bind TvControlServer on port $port", e)
            running.set(false)
            false
        }
    }

    fun stop() {
        running.set(false)
        runCatching { serverSocket?.close() }
        serverSocket = null
        runCatching { acceptThread?.join(300L) }
        acceptThread = null
    }

    private fun handleClient(client: Socket) {
        client.use { sock ->
            sock.soTimeout = 3000
            val req = readHttpRequest(sock.getInputStream()) ?: return
            val remoteIp = (sock.inetAddress as? Inet4Address)?.hostAddress
                ?: sock.inetAddress?.hostAddress
                ?: "unknown"

            val (status, statusText, responseBody) = when {
                req.method == "GET" && req.path == "/nayutacast/info" -> {
                    val info = TvInfoResponse(
                        app = "NayutaCast TV",
                        name = deviceNameProvider(),
                        model = Build.MODEL.orEmpty(),
                        manufacturer = Build.MANUFACTURER.orEmpty(),
                        platform = "android-tv",
                        version = appVersion,
                        srtReady = true,
                        casting = isCastingProvider(),
                    )
                    Triple(200, "OK", json.encodeToString(info))
                }

                req.method == "POST" && req.path == "/nayutacast/start" -> {
                    val parsed = runCatching {
                        val bodyText = req.body.toString(Charsets.UTF_8)
                        json.decodeFromString<MediaStreamDescription>(bodyText)
                    }
                    val desc = parsed.getOrNull()
                    if (desc == null) {
                        Triple(400, "Bad Request", """{"ok":false,"error":"invalid MediaStreamDescription"}""")
                    } else {
                        Log.i("nayutacast", "HTTP /nayutacast/start from $remoteIp -> ${desc.transport.strategy}")
                        onStartStream(remoteIp, desc)
                        Triple(200, "OK", """{"ok":true}""")
                    }
                }

                req.method == "POST" && req.path == "/nayutacast/stop" -> {
                    Log.i("nayutacast", "HTTP /nayutacast/stop from $remoteIp")
                    onStopStream()
                    Triple(200, "OK", """{"ok":true}""")
                }

                else -> Triple(404, "Not Found", """{"ok":false,"error":"not found"}""")
            }

            val bodyBytes = responseBody.toByteArray(Charsets.UTF_8)
            val header = buildString {
                append("HTTP/1.1 $status $statusText\r\n")
                append("Content-Type: application/json; charset=utf-8\r\n")
                append("Content-Length: ${bodyBytes.size}\r\n")
                append("Connection: close\r\n\r\n")
            }.toByteArray(Charsets.US_ASCII)

            val out = sock.getOutputStream()
            out.write(header)
            out.write(bodyBytes)
            out.flush()
        }
    }

    private data class HttpReq(
        val method: String,
        val path: String,
        val body: ByteArray,
    )

    private fun readHttpRequest(input: InputStream): HttpReq? {
        val buf = ByteArrayOutputStream(1024)
        var headerEnd = -1
        val tmp = ByteArray(1)
        var lastFour = 0

        while (buf.size() < MAX_HEADER_BYTES) {
            val n = input.read(tmp)
            if (n <= 0) break
            val b = tmp[0].toInt() and 0xFF
            buf.write(b)
            lastFour = (lastFour shl 8) or b
            if (lastFour == 0x0D0A0D0A) {
                headerEnd = buf.size()
                break
            }
        }
        if (headerEnd <= 0) return null

        val headerStr = buf.toByteArray().toString(Charsets.ISO_8859_1)
        val lines = headerStr.split("\r\n")
        val requestLine = lines.firstOrNull()?.split(" ") ?: return null
        if (requestLine.size < 2) return null

        val method = requestLine[0].uppercase()
        val rawTarget = requestLine[1].substringBefore('?')
        var contentLength = 0
        for (line in lines.drop(1)) {
            val colon = line.indexOf(':')
            if (colon > 0) {
                val key = line.substring(0, colon).trim()
                if (key.equals("Content-Length", ignoreCase = true)) {
                    contentLength = line.substring(colon + 1).trim().toIntOrNull()
                        ?.coerceIn(0, MAX_BODY_BYTES)
                        ?: 0
                }
            }
        }

        val body = if (contentLength > 0) {
            val out = ByteArray(contentLength)
            var offset = 0
            while (offset < contentLength) {
                val read = input.read(out, offset, contentLength - offset)
                if (read < 0) break
                offset += read
            }
            if (offset == contentLength) out else out.copyOf(offset)
        } else {
            ByteArray(0)
        }

        return HttpReq(method, rawTarget, body)
    }

    @Serializable
    data class TvInfoResponse(
        val app: String,
        val name: String,
        val model: String,
        val manufacturer: String,
        val platform: String,
        val version: String,
        @SerialName("srt_ready") val srtReady: Boolean,
        val casting: Boolean,
    )

    companion object {
        const val CONTROL_PORT = 60344
        private const val MAX_HEADER_BYTES = 16 * 1024
        private const val MAX_BODY_BYTES = 512 * 1024

        fun listLocalIpv4Addresses(): List<String> {
            val result = linkedSetOf<String>()
            runCatching {
                val ifaces = NetworkInterface.getNetworkInterfaces() ?: return emptyList()
                while (ifaces.hasMoreElements()) {
                    val iface = ifaces.nextElement()
                    if (!iface.isUp || iface.isLoopback || iface.isVirtual) continue
                    val name = iface.name.orEmpty().lowercase()
                    if (name.startsWith("tun") || name.startsWith("ppp") || name.startsWith("dummy")) {
                        continue
                    }
                    val addrs = iface.inetAddresses
                    while (addrs.hasMoreElements()) {
                        val addr = addrs.nextElement()
                        if (addr is Inet4Address && !addr.isLoopbackAddress && !addr.isLinkLocalAddress) {
                            addr.hostAddress?.let { result.add(it) }
                        }
                    }
                }
            }
            return result.toList()
        }
    }
}
