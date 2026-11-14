package com.nayutacast.app

import android.content.ComponentName
import android.content.Intent
import android.content.ServiceConnection
import android.graphics.PixelFormat
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.util.Log
import android.view.KeyEvent
import android.view.Surface
import android.view.SurfaceHolder
import android.view.SurfaceView
import android.view.WindowManager
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ExperimentalLayoutApi
import androidx.compose.foundation.layout.FlowRow
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.material3.darkColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.focus.FocusRequester
import androidx.compose.ui.focus.focusRequester
import androidx.compose.ui.focus.onFocusChanged
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.core.view.WindowCompat
import androidx.core.view.WindowInsetsCompat
import androidx.core.view.WindowInsetsControllerCompat
import com.nayutacast.MediaStreamDescription
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.delay
import java.util.concurrent.atomic.AtomicLong

private data class ActiveCastSession(
    val token: Long,
    val sourceId: String,
    val senderName: String,
    val senderIp: String,
    val protocolLabel: String,
    val description: MediaStreamDescription,
)

class MainActivity : ComponentActivity() {
    private val mainHandler = Handler(Looper.getMainLooper())
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.IO)
    private val sessionSeq = AtomicLong(0L)

    private lateinit var settings: Settings
    private lateinit var deviceManager: DeviceManager
    private lateinit var tvControlServer: TvControlServer

    private var serviceBinder: NayutacastCoreService.ServiceBinder? = null
    private var isServiceBound = false

    private var surfaceViewRef: SurfaceView? = null
    private var readySurface: Surface? = null
    private var startedSessionToken: Long = -1L

    private var tvName by mutableStateOf("NayutaCast TV")
    private var localIps by mutableStateOf<List<String>>(emptyList())
    private var isP2pReady by mutableStateOf(false)
    private var isHttpControlReady by mutableStateOf(false)
    private var activeSession by mutableStateOf<ActiveCastSession?>(null)
    private var videoWidth by mutableIntStateOf(1920)
    private var videoHeight by mutableIntStateOf(1080)
    private var isHudVisible by mutableStateOf(false)
    private var statusMessage by mutableStateOf<String?>(null)
    private var showRenameDialog by mutableStateOf(false)

    private val serviceConnection = object : ServiceConnection {
        override fun onServiceConnected(name: ComponentName?, service: IBinder?) {
            serviceBinder = service as? NayutacastCoreService.ServiceBinder
            maybeAttachPendingStream()
        }

        override fun onServiceDisconnected(name: ComponentName?) {
            serviceBinder = null
            startedSessionToken = -1L
        }
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        settings = Settings(this)
        tvName = settings.value.system.name
        localIps = TvControlServer.listLocalIpv4Addresses()

        bindService(
            Intent(this, NayutacastCoreService::class.java),
            serviceConnection,
            BIND_AUTO_CREATE
        )
        isServiceBound = true

        tvControlServer = TvControlServer(
            appVersion = BuildConfig.VERSION_NAME,
            deviceNameProvider = { tvName },
            isCastingProvider = { activeSession != null },
            onStartStream = { senderIp, desc ->
                requestStartStream(
                    sourceId = "http:$senderIp",
                    senderName = "Desktop ($senderIp)",
                    senderIp = senderIp,
                    protocolLabel = "Direct SRT · Port ${TvControlServer.CONTROL_PORT}",
                    description = desc,
                )
            },
            onStopStream = {
                mainHandler.post {
                    stopActiveCast(reason = null)
                }
            }
        )
        isHttpControlReady = tvControlServer.start()

        deviceManager = DeviceManager(
            context = this,
            settings = settings,
            scope = scope,
            onP2pReadyChanged = { ready ->
                mainHandler.post { isP2pReady = ready }
            },
            onStreamStart = { peerId, senderName, senderIp, desc ->
                requestStartStream(
                    sourceId = "p2p:$peerId",
                    senderName = senderName,
                    senderIp = senderIp,
                    protocolLabel = "libp2p + SRT Native",
                    description = desc,
                )
            },
            onStreamStop = { peerId ->
                mainHandler.post {
                    val cur = activeSession ?: return@post
                    if (cur.sourceId == "p2p:$peerId") {
                        stopActiveCast(reason = null)
                    }
                }
            },
            onPeerOffline = { peerId ->
                mainHandler.post {
                    val cur = activeSession ?: return@post
                    if (cur.sourceId == "p2p:$peerId") {
                        stopActiveCast(reason = "Koneksi pengirim terputus")
                    }
                }
            }
        )

        setContent {
            NayutacastTvApp()
        }
    }

    override fun onDestroy() {
        stopActiveCast(reason = null)
        if (isServiceBound) {
            runCatching { unbindService(serviceConnection) }
            isServiceBound = false
        }
        runCatching { tvControlServer.stop() }
        runCatching { deviceManager.release() }
        scope.cancel()
        super.onDestroy()
    }

    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
        if (activeSession != null && event.action == KeyEvent.ACTION_DOWN) {
            when (event.keyCode) {
                KeyEvent.KEYCODE_BACK -> {
                    isHudVisible = !isHudVisible
                    return true
                }

                KeyEvent.KEYCODE_DPAD_CENTER,
                KeyEvent.KEYCODE_ENTER,
                KeyEvent.KEYCODE_MENU,
                KeyEvent.KEYCODE_INFO,
                KeyEvent.KEYCODE_MEDIA_PLAY_PAUSE -> {
                    if (!isHudVisible) {
                        isHudVisible = true
                        return true
                    }
                }
            }
        }
        return super.dispatchKeyEvent(event)
    }

    private fun requestStartStream(
        sourceId: String,
        senderName: String,
        senderIp: String,
        protocolLabel: String,
        description: MediaStreamDescription
    ) {
        val token = sessionSeq.incrementAndGet()
        mainHandler.post {
            val existing = activeSession
            if (existing != null &&
                existing.description.id == description.id &&
                existing.description.transport.strategy.address == description.transport.strategy.address &&
                startedSessionToken == existing.token
            ) {
                return@post
            }

            if (startedSessionToken != -1L) {
                serviceBinder?.stopReceiver()
                startedSessionToken = -1L
            }

            val width = description.video?.size?.width?.takeIf { it > 0 } ?: 1920
            val height = description.video?.size?.height?.takeIf { it > 0 } ?: 1080
            videoWidth = width
            videoHeight = height
            statusMessage = null
            showRenameDialog = false
            isHudVisible = false
            activeSession = ActiveCastSession(
                token = token,
                sourceId = sourceId,
                senderName = senderName,
                senderIp = senderIp,
                protocolLabel = protocolLabel,
                description = description,
            )
            applyFullscreenMode(true)
            maybeAttachPendingStream()
        }
    }

    private fun maybeAttachPendingStream() {
        val session = activeSession ?: return
        val binder = serviceBinder ?: return
        val surface = readySurface ?: return
        if (!surface.isValid || startedSessionToken == session.token) {
            return
        }

        startedSessionToken = session.token
        Log.i(
            "nayutacast",
            "Starting SRT receiver session=${session.token} addr=${session.description.transport.strategy.address}"
        )

        val ok = binder.createReceiver(
            surface = surface,
            description = session.description,
            observer = object : NayutacastCoreService.Observer() {
                override fun onVideoSizeChanged(width: Int, height: Int) {
                    mainHandler.post {
                        if (activeSession?.token == session.token && width > 0 && height > 0) {
                            videoWidth = width
                            videoHeight = height
                        }
                    }
                }

                override fun onClosed() {
                    mainHandler.post {
                        if (activeSession?.token == session.token) {
                            stopActiveCast(reason = "Streaming selesai")
                        }
                    }
                }
            }
        )

        if (!ok) {
            stopActiveCast(reason = "Gagal menginisialisasi decoder video/SRT pada TV")
        }
    }

    private fun stopActiveCast(reason: String?) {
        startedSessionToken = -1L
        serviceBinder?.stopReceiver()
        activeSession = null
        isHudVisible = false
        if (reason != null) {
            statusMessage = reason
        }
        applyFullscreenMode(false)
    }

    private fun applyFullscreenMode(fullscreen: Boolean) {
        if (fullscreen) {
            window.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        } else {
            window.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        }
        WindowCompat.setDecorFitsSystemWindows(window, !fullscreen)
        val controller = WindowInsetsControllerCompat(window, window.decorView)
        if (fullscreen) {
            controller.hide(WindowInsetsCompat.Type.systemBars())
            controller.systemBarsBehavior =
                WindowInsetsControllerCompat.BEHAVIOR_SHOW_TRANSIENT_BARS_BY_SWIPE
        } else {
            controller.show(WindowInsetsCompat.Type.systemBars())
        }
    }

    @Composable
    private fun NayutacastTvApp() {
        val colors = darkColorScheme(
            primary = Color(0xFF38BDF8),
            onPrimary = Color(0xFF061323),
            background = Color(0xFF0A1020),
            surface = Color(0xFF141E36),
            onBackground = Color(0xFFF1F5F9),
            onSurface = Color(0xFFF1F5F9),
        )

        MaterialTheme(colorScheme = colors) {
            LaunchedEffect(activeSession == null) {
                while (activeSession == null) {
                    localIps = TvControlServer.listLocalIpv4Addresses()
                    delay(4000L)
                }
            }

            val session = activeSession
            if (session != null) {
                TvPlayerScreen(session)
            } else {
                TvStandbyScreen()
            }

            if (showRenameDialog && session == null) {
                RenameTvDialog(
                    currentName = tvName,
                    onDismiss = { showRenameDialog = false },
                    onSave = { updated ->
                        settings.updateDeviceName(updated)
                        tvName = settings.value.system.name
                        deviceManager.refresh()
                        showRenameDialog = false
                    }
                )
            }
        }
    }

    @Composable
    private fun TvStandbyScreen() {
        val refreshFocus = remember { FocusRequester() }
        val scrollState = rememberScrollState()

        LaunchedEffect(Unit) {
            runCatching { refreshFocus.requestFocus() }
        }

        val bgBrush = remember {
            Brush.linearGradient(
                listOf(
                    Color(0xFF080E1C),
                    Color(0xFF111C33),
                    Color(0xFF0B1A2E),
                )
            )
        }

        BoxWithConstraints(
            modifier = Modifier
                .fillMaxSize()
                .background(bgBrush)
        ) {
            val isWideTv = maxWidth >= 700.dp
            val outerPadH = if (isWideTv) 48.dp else 20.dp
            val outerPadV = if (isWideTv) 32.dp else 20.dp

            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .verticalScroll(scrollState)
                    .padding(horizontal = outerPadH, vertical = outerPadV),
                verticalArrangement = Arrangement.spacedBy(20.dp)
            ) {
                // Header bar
                Row(
                    modifier = Modifier.fillMaxWidth(),
                    horizontalArrangement = Arrangement.SpaceBetween,
                    verticalAlignment = Alignment.CenterVertically
                ) {
                    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                        Text(
                            text = "NayutaCast TV Receiver",
                            fontSize = if (isWideTv) 30.sp else 24.sp,
                            fontWeight = FontWeight.Bold,
                            color = Color.White
                        )
                        Text(
                            text = "Low-Latency Native SRT Screen Mirroring (${Build.SUPPORTED_ABIS.firstOrNull() ?: "Android"})",
                            fontSize = 14.sp,
                            color = Color(0xFF94A3B8)
                        )
                    }

                    Row(
                        horizontalArrangement = Arrangement.spacedBy(10.dp),
                        verticalAlignment = Alignment.CenterVertically
                    ) {
                        StatusChip(
                            label = if (isHttpControlReady) {
                                "LAN :${TvControlServer.CONTROL_PORT}"
                            } else {
                                "LAN Offline"
                            },
                            active = isHttpControlReady
                        )
                        StatusChip(
                            label = if (isP2pReady) "P2P Ready" else "LAN Direct",
                            active = isP2pReady || isHttpControlReady
                        )
                    }
                }

                // Device Identity + IP Card
                Surface(
                    modifier = Modifier.fillMaxWidth(),
                    shape = RoundedCornerShape(18.dp),
                    color = Color(0xCC16223D),
                    border = BorderStroke(1.dp, Color(0x3338BDF8))
                ) {
                    if (isWideTv) {
                        Row(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(24.dp),
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.CenterVertically
                        ) {
                            IdentityInfoBlock(
                                tvName = tvName,
                                ips = localIps,
                                modifier = Modifier.weight(1f)
                            )
                            Spacer(modifier = Modifier.width(24.dp))
                            ActionButtonsColumn(
                                refreshFocus = refreshFocus,
                                onRefresh = {
                                    localIps = TvControlServer.listLocalIpv4Addresses()
                                    deviceManager.refresh()
                                    statusMessage = "Status jaringan & discovery diperbarui"
                                },
                                onRename = { showRenameDialog = true }
                            )
                        }
                    } else {
                        Column(
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(20.dp),
                            verticalArrangement = Arrangement.spacedBy(16.dp)
                        ) {
                            IdentityInfoBlock(tvName = tvName, ips = localIps)
                            ActionButtonsColumn(
                                refreshFocus = refreshFocus,
                                onRefresh = {
                                    localIps = TvControlServer.listLocalIpv4Addresses()
                                    deviceManager.refresh()
                                    statusMessage = "Status jaringan & discovery diperbarui"
                                },
                                onRename = { showRenameDialog = true }
                            )
                        }
                    }
                }

                // Connection Guide
                Surface(
                    modifier = Modifier.fillMaxWidth(),
                    shape = RoundedCornerShape(16.dp),
                    color = Color(0x99121C32),
                    border = BorderStroke(1.dp, Color(0x2294A3B8))
                ) {
                    Column(
                        modifier = Modifier.padding(20.dp),
                        verticalArrangement = Arrangement.spacedBy(10.dp)
                    ) {
                        Text(
                            text = "Cara Menyambung dari Laptop / PC",
                            fontSize = 16.sp,
                            fontWeight = FontWeight.SemiBold,
                            color = Color(0xFF7DD3FC)
                        )
                        Text(
                            text = "1. Pastikan Android TV dan PC terhubung ke Wi-Fi / LAN yang sama.",
                            fontSize = 14.sp,
                            color = Color(0xFFCBD5E1)
                        )
                        Text(
                            text = "2. Buka NayutaCast Desktop -> klik Refresh. Perangkat \"$tvName\" otomatis muncul sebagai target \"NayutaCast\".",
                            fontSize = 14.sp,
                            color = Color(0xFFCBD5E1)
                        )
                        Text(
                            text = "3. Jika tidak muncul otomatis, klik Tambah Manual / IP TV dan masukkan alamat IP di atas.",
                            fontSize = 14.sp,
                            color = Color(0xFFCBD5E1)
                        )
                    }
                }

                statusMessage?.let { msg ->
                    Surface(
                        shape = RoundedCornerShape(12.dp),
                        color = Color(0xFF1E293B),
                        border = BorderStroke(1.dp, Color(0x4438BDF8))
                    ) {
                        Text(
                            text = msg,
                            modifier = Modifier.padding(horizontal = 16.dp, vertical = 10.dp),
                            fontSize = 13.sp,
                            color = Color(0xFFE2E8F0)
                        )
                    }
                }
            }
        }
    }

    @Composable
    private fun IdentityInfoBlock(
        tvName: String,
        ips: List<String>,
        modifier: Modifier = Modifier
    ) {
        Column(
            modifier = modifier,
            verticalArrangement = Arrangement.spacedBy(8.dp)
        ) {
            Text(
                text = "NAMA PERANGKAT TV",
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                letterSpacing = 1.sp,
                color = Color(0xFF38BDF8)
            )
            Text(
                text = tvName,
                fontSize = 24.sp,
                fontWeight = FontWeight.Bold,
                color = Color.White
            )
            Spacer(modifier = Modifier.height(4.dp))
            Text(
                text = "ALAMAT IP JARINGAN",
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                letterSpacing = 1.sp,
                color = Color(0xFF94A3B8)
            )
            Text(
                text = if (ips.isEmpty()) {
                    "Belum terhubung ke Wi-Fi / Ethernet"
                } else {
                    ips.joinToString("  ·  ")
                },
                fontSize = 20.sp,
                fontWeight = FontWeight.Medium,
                fontFamily = FontFamily.Monospace,
                color = if (ips.isEmpty()) Color(0xFFFBBF24) else Color(0xFF4ADE80)
            )
            Text(
                text = "Port Kontrol: ${TvControlServer.CONTROL_PORT} · Codec Decoder: H.264 + Opus Hardware",
                fontSize = 13.sp,
                color = Color(0xFF94A3B8)
            )
        }
    }

    @Composable
    private fun ActionButtonsColumn(
        refreshFocus: FocusRequester,
        onRefresh: () -> Unit,
        onRename: () -> Unit,
    ) {
        Column(
            verticalArrangement = Arrangement.spacedBy(12.dp),
            horizontalAlignment = Alignment.End
        ) {
            TvFocusButton(
                label = "Refresh Jaringan",
                isPrimary = true,
                modifier = Modifier.focusRequester(refreshFocus),
                onClick = onRefresh
            )
            TvFocusButton(
                label = "Ubah Nama TV",
                isPrimary = false,
                onClick = onRename
            )
        }
    }

    @Composable
    private fun TvPlayerScreen(session: ActiveCastSession) {
        LaunchedEffect(isHudVisible, session.token) {
            if (isHudVisible) {
                delay(6000L)
                isHudVisible = false
            }
        }

        val closeFocus = remember { FocusRequester() }
        LaunchedEffect(isHudVisible) {
            if (isHudVisible) {
                runCatching { closeFocus.requestFocus() }
            }
        }

        BoxWithConstraints(
            modifier = Modifier
                .fillMaxSize()
                .background(Color.Black),
            contentAlignment = Alignment.Center
        ) {
            val aspectRatio = (videoWidth.toFloat() / videoHeight.coerceAtLeast(1).toFloat())
                .coerceIn(0.4f, 4.0f)
            val containerRatio = maxWidth.value / maxHeight.value.coerceAtLeast(1f)
            val surfaceSizeModifier = if (containerRatio > aspectRatio) {
                Modifier.size(width = maxHeight * aspectRatio, height = maxHeight)
            } else {
                Modifier.size(width = maxWidth, height = maxWidth / aspectRatio)
            }

            AndroidView(
                modifier = surfaceSizeModifier,
                factory = { ctx ->
                    SurfaceView(ctx).apply {
                        holder.setFormat(PixelFormat.RGBA_8888)
                        holder.addCallback(object : SurfaceHolder.Callback {
                            override fun surfaceCreated(holder: SurfaceHolder) {
                                readySurface = holder.surface
                                maybeAttachPendingStream()
                            }

                            override fun surfaceChanged(
                                holder: SurfaceHolder,
                                format: Int,
                                width: Int,
                                height: Int
                            ) {
                                readySurface = holder.surface
                            }

                            override fun surfaceDestroyed(holder: SurfaceHolder) {
                                readySurface = null
                                if (startedSessionToken != -1L) {
                                    startedSessionToken = -1L
                                    serviceBinder?.stopReceiver()
                                }
                            }
                        })
                        setOnClickListener {
                            isHudVisible = !isHudVisible
                        }
                        surfaceViewRef = this
                    }
                },
                onRelease = { view ->
                    if (surfaceViewRef === view) {
                        surfaceViewRef = null
                    }
                    readySurface = null
                }
            )

            AnimatedVisibility(
                visible = isHudVisible,
                enter = fadeIn(),
                exit = fadeOut(),
                modifier = Modifier
                    .align(Alignment.TopEnd)
                    .padding(32.dp)
            ) {
                Surface(
                    shape = RoundedCornerShape(16.dp),
                    color = Color(0xDE0B1120),
                    border = BorderStroke(1.dp, Color(0x5538BDF8))
                ) {
                    Column(
                        modifier = Modifier.padding(18.dp),
                        verticalArrangement = Arrangement.spacedBy(10.dp)
                    ) {
                        Text(
                            text = session.senderName,
                            fontSize = 16.sp,
                            fontWeight = FontWeight.Bold,
                            color = Color.White
                        )
                        Text(
                            text = "${session.protocolLabel} · ${videoWidth}x${videoHeight}",
                            fontSize = 13.sp,
                            color = Color(0xFF94A3B8)
                        )
                        Row(horizontalArrangement = Arrangement.spacedBy(10.dp)) {
                            TvFocusButton(
                                label = "Hentikan Cast",
                                isPrimary = true,
                                danger = true,
                                modifier = Modifier.focusRequester(closeFocus),
                                onClick = { stopActiveCast(reason = "Casting dihentikan") }
                            )
                            TvFocusButton(
                                label = "Tutup Menu",
                                isPrimary = false,
                                onClick = { isHudVisible = false }
                            )
                        }
                    }
                }
            }
        }
    }

    @Composable
    private fun StatusChip(label: String, active: Boolean) {
        val bg = if (active) Color(0x2622C55E) else Color(0x26F59E0B)
        val fg = if (active) Color(0xFF4ADE80) else Color(0xFFFBBF24)
        Row(
            modifier = Modifier
                .background(bg, RoundedCornerShape(999.dp))
                .border(1.dp, fg.copy(alpha = 0.45f), RoundedCornerShape(999.dp))
                .padding(horizontal = 12.dp, vertical = 6.dp),
            verticalAlignment = Alignment.CenterVertically,
            horizontalArrangement = Arrangement.spacedBy(6.dp)
        ) {
            Box(
                modifier = Modifier
                    .size(8.dp)
                    .background(fg, CircleShape)
            )
            Text(
                text = label,
                fontSize = 12.sp,
                fontWeight = FontWeight.SemiBold,
                color = fg
            )
        }
    }

    @Composable
    private fun TvFocusButton(
        label: String,
        isPrimary: Boolean,
        modifier: Modifier = Modifier,
        danger: Boolean = false,
        onClick: () -> Unit,
    ) {
        var focused by remember { mutableStateOf(false) }
        val borderColor = if (focused) Color(0xFFFFFFFF) else Color.Transparent
        val primaryContainer = if (danger) Color(0xFFDC2626) else Color(0xFF38BDF8)
        val primaryContent = if (danger) Color.White else Color(0xFF061323)

        if (isPrimary) {
            Button(
                onClick = onClick,
                modifier = modifier
                    .onFocusChanged { focused = it.isFocused }
                    .border(
                        width = if (focused) 2.5.dp else 0.dp,
                        color = borderColor,
                        shape = RoundedCornerShape(10.dp)
                    ),
                shape = RoundedCornerShape(10.dp),
                colors = ButtonDefaults.buttonColors(
                    containerColor = primaryContainer,
                    contentColor = primaryContent
                )
            ) {
                Text(text = label, fontWeight = FontWeight.SemiBold)
            }
        } else {
            OutlinedButton(
                onClick = onClick,
                modifier = modifier
                    .onFocusChanged { focused = it.isFocused }
                    .border(
                        width = if (focused) 2.5.dp else 1.dp,
                        color = if (focused) Color(0xFF38BDF8) else Color(0x4494A3B8),
                        shape = RoundedCornerShape(10.dp)
                    ),
                shape = RoundedCornerShape(10.dp),
                border = null
            ) {
                Text(text = label, color = Color(0xFFE2E8F0))
            }
        }
    }

    @OptIn(ExperimentalLayoutApi::class)
    @Composable
    private fun RenameTvDialog(
        currentName: String,
        onDismiss: () -> Unit,
        onSave: (String) -> Unit,
    ) {
        var draft by remember { mutableStateOf(currentName) }
        val presets = remember {
            listOf(
                Settings.buildDefaultDeviceName(),
                "Polytron Android TV",
                "TV Ruang Tamu",
                "TV Kamar"
            ).distinct()
        }

        AlertDialog(
            onDismissRequest = onDismiss,
            title = { Text("Ubah Nama TV", color = Color.White) },
            text = {
                Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
                    Text(
                        text = "Pilih cepat dengan remote D-pad atau ketik nama:",
                        fontSize = 13.sp,
                        color = Color(0xFF94A3B8)
                    )
                    FlowRow(
                        horizontalArrangement = Arrangement.spacedBy(8.dp),
                        verticalArrangement = Arrangement.spacedBy(8.dp)
                    ) {
                        for (preset in presets) {
                            TvFocusButton(
                                label = preset,
                                isPrimary = false,
                                onClick = { onSave(preset) }
                            )
                        }
                    }
                    OutlinedTextField(
                        value = draft,
                        onValueChange = { draft = it },
                        singleLine = true,
                        label = { Text("Nama kustom") },
                        modifier = Modifier.fillMaxWidth()
                    )
                }
            },
            confirmButton = {
                Button(onClick = { onSave(draft) }) {
                    Text("Simpan")
                }
            },
            dismissButton = {
                TextButton(onClick = onDismiss) {
                    Text("Batal")
                }
            }
        )
    }
}
