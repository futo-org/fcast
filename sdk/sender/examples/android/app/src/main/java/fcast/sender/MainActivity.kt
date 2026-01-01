package fcast.sender

import android.content.Context
import android.os.Build
import android.os.Bundle
import android.util.Log
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.*
import androidx.compose.foundation.layout.*
import androidx.compose.material3.*
import androidx.compose.runtime.*
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.foundation.shape.CutCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import androidx.lifecycle.lifecycleScope
import coil3.compose.AsyncImage
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import com.prof18.rssparser.RssParser
import com.prof18.rssparser.model.RssChannel
import com.prof18.rssparser.model.RssItem
import fcast.sender.ui.theme.FCastSenderTheme
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import org.fcast.sender_sdk.*
import org.fcast.sender_sdk.CastContext as RustCastContext

class DiscoveryEventHandler(
    private val devices: MutableState<List<DeviceInfo>>,
) : DeviceDiscovererEventHandler {
    override fun deviceAvailable(deviceInfo: DeviceInfo) {
        val dev = devices.value.find { it.name == deviceInfo.name }
        if (dev != null) {
            dev.addresses = deviceInfo.addresses
            dev.port = deviceInfo.port
        } else {
            devices.value += deviceInfo
        }
    }

    override fun deviceChanged(deviceInfo: DeviceInfo) {
        devices.value.find { it.name == deviceInfo.name }?.let {
            it.addresses = deviceInfo.addresses
            it.port = deviceInfo.port
        }
    }

    override fun deviceRemoved(deviceName: String) {
        devices.value.filter { it.name != deviceName }.let {
            devices.value = it
        }
    }
}

enum class ConnectionState {
    Connecting, Connected, Reconnecting, Disconnected
}

data class State(
    var connectionState: MutableState<ConnectionState> = mutableStateOf(ConnectionState.Connecting),
    var volume: MutableState<Double> = mutableDoubleStateOf(1.0),
    var playbackState: MutableState<PlaybackState> = mutableStateOf(PlaybackState.IDLE),
    var time: MutableState<Double> = mutableDoubleStateOf(0.0),
    var duration: MutableState<Double> = mutableDoubleStateOf(0.0),
    var speed: MutableState<Double> = mutableDoubleStateOf(1.0),
    var contentType: MutableState<String> = mutableStateOf(""),
    var localAddress: IpAddr? = null,
) {
    fun reset() {
        connectionState.value = ConnectionState.Connecting
        volume.value = 1.0
        playbackState.value = PlaybackState.IDLE
        time.value = 0.0
        duration.value = 0.0
        speed.value = 1.0
        contentType.value = ""
        localAddress = null
    }
}

class KtCastContext() {
    class EventHandler(
        val ctx: KtCastContext, var generation: Int
    ) : DeviceEventHandler {
        override fun connectionStateChanged(state: DeviceConnectionState) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Connection state changed: $state")
            when (state) {
                is DeviceConnectionState.Connected -> {
                    ctx.state.localAddress = state.localAddr
                    ctx.state.connectionState.value = ConnectionState.Connected
                    ctx.onConnected?.invoke()
                }

                DeviceConnectionState.Connecting -> ctx.state.connectionState.value =
                    ConnectionState.Connecting

                DeviceConnectionState.Reconnecting -> ctx.state.connectionState.value =
                    ConnectionState.Reconnecting

                DeviceConnectionState.Disconnected -> {
                    ctx.state.connectionState.value = ConnectionState.Disconnected
                    ctx.onDisconnected?.invoke()
                }
            }
        }

        override fun volumeChanged(volume: Double) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Volume changed: $volume")
            ctx.state.volume.value = volume
        }

        override fun timeChanged(time: Double) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Time changed: $time")
            ctx.state.time.value = time
        }

        override fun playbackStateChanged(state: PlaybackState) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Playback state changed: $state")
            ctx.state.playbackState.value = state
        }

        override fun durationChanged(duration: Double) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Duration changed: $duration")
            ctx.state.duration.value = duration
        }

        override fun speedChanged(speed: Double) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Speed changed: $speed")
            ctx.state.speed.value = speed
        }

        override fun sourceChanged(source: Source) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Source changed: $source")
            when (source) {
                is Source.Url -> {
                    ctx.state.contentType.value = source.contentType
                }

                else -> {
                    ctx.state.contentType.value = ""
                }
            }
        }

        override fun keyEvent(event: KeyEvent) {}

        override fun mediaEvent(event: MediaEvent) {}

        override fun playbackError(message: String) {
            if (generation != ctx.currentGeneration) return
            Log.v(TAG, "Playback error: $message")
        }
    }

    val context = RustCastContext()
    lateinit var deviceDiscoverer: NsdDeviceDiscoverer
    var activeDevice: MutableState<CastingDevice?> = mutableStateOf(null)
    val devices: MutableState<List<DeviceInfo>> = mutableStateOf(listOf())
    val state = State()
    var onQRRequested: (() -> Unit)? = null
    var onConnected: (() -> Unit)? = null
    var onDisconnected: (() -> Unit)? = null
    private val minMillisBetweenVolumeChanges = 250
    private var lastVolumeChange: Long = System.currentTimeMillis()
    private var currentGeneration: Int = 0

    init {
        initLogger(LogLevelFilter.DEBUG)
    }

    fun startDiscovery(ctx: Context) {
        deviceDiscoverer = NsdDeviceDiscoverer(ctx, DiscoveryEventHandler(devices))
    }

    fun connect(deviceInfo: DeviceInfo) {
        val device = context.createDeviceFromInfo(deviceInfo)
        currentGeneration += 1
        val prevDev = activeDevice.value
        activeDevice.value = null
        if (prevDev != null) {
            try {
                prevDev.disconnect()
            } catch (e: Throwable) {
                Log.e(TAG, "Failed to disconnect active device: $e")
            }
        }
        state.reset()
        try {
            device.connect(
                ApplicationInfo(
                    name = "FCast SDK example android",
                    version = "1",
                    displayName = "${Build.MANUFACTURER} ${Build.MODEL}",
                ),
                eventHandler = EventHandler(this, currentGeneration),
                reconnectIntervalMillis = 1000.toULong(),
            )
            activeDevice.value = device
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to connect to device: $e")
        }
    }

    fun setVolume(volume: Double, force: Boolean = false) {
        val now = System.currentTimeMillis()
        if (!force && now - lastVolumeChange < minMillisBetweenVolumeChanges) {
            return
        }
        try {
            activeDevice.value?.changeVolume(volume)
            lastVolumeChange = now
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to set volume to $volume: $e")
        }
    }

    fun disconnect() {
        try {
            activeDevice.value?.disconnect()
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to disconnect: $e")
        }
        activeDevice.value = null
    }

    fun qrScanComplete(result: String) {
        deviceInfoFromUrl(result)?.let { deviceInfo ->
            connect(deviceInfo)
        }
    }

    companion object {
        const val TAG = "KtCastContext"
    }
}

class MainActivity : ComponentActivity() {
    private val castContext = KtCastContext()
    private val barcodeLauncher = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let {
            castContext.qrScanComplete(it)
        }
    }
    private val rssParser = RssParser()
    private var rssChannel: MutableState<RssChannel?> = mutableStateOf(null)
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        castContext.startDiscovery(this)
        castContext.onQRRequested = {
            barcodeLauncher.launch(ScanOptions().setOrientationLocked(false))
        }
        castContext.onConnected = {
            Log.i(TAG, "Connected")
        }
        castContext.onDisconnected = {
            Log.i(TAG, "Disconnected")
        }
        enableEdgeToEdge()
        setContent {
            FCastSenderTheme {
                Scaffold(
                    modifier = Modifier
                        .fillMaxSize()
                        .systemBarsPadding()
                ) { innerPadding ->
                    View(
                        Modifier.padding(innerPadding), castContext, rssChannel
                    ) { url ->
                        lifecycleScope.launch(Dispatchers.IO) {
                            val channel = rssParser.getRssChannel(url)
                            rssChannel.value = channel
                        }
                    }
                }
            }
        }
    }

    companion object {
        const val TAG = "MainActivity"
    }
}

@Composable
fun FeedItem(
    currentlyPlaying: MutableState<String?>,
    castContext: KtCastContext,
    fallbackImage: String?,
    item: RssItem
) {
    val thisId = item.rawEnclosure?.url
    val thisIsPlaying = currentlyPlaying.value == thisId
    Row(
        verticalAlignment = Alignment.CenterVertically,
        modifier = Modifier
            .fillMaxWidth()
            .height(45.dp)
            .border(
                BorderStroke(
                    width = 3.dp, color = if (thisIsPlaying) MaterialTheme.colorScheme.secondary
                    else MaterialTheme.colorScheme.background
                )
            )
    ) {
        val itunes = item.itunesItemData
        if (itunes != null && itunes.image != null) {
            AsyncImage(model = itunes.image, contentDescription = null)
        } else if (fallbackImage != null) {
            AsyncImage(model = fallbackImage, contentDescription = null)
        }
        Text(
            item.title ?: "n/a",
            maxLines = 1,
            overflow = TextOverflow.Ellipsis,
            modifier = Modifier.weight(1f)
        )

        val isConnected =
            castContext.state.connectionState.value == ConnectionState.Connected
        var playIconTint = MaterialTheme.colorScheme.onBackground
        if (!isConnected) {
            playIconTint = playIconTint.copy(alpha = 0.5f)
        }

        val raw = item.rawEnclosure ?: return@Row
        val type = raw.type
        val url = raw.url
        if (type == null || url == null) {
            return@Row
        }
        val pbState = castContext.state.playbackState.value

        if (thisIsPlaying && (pbState == PlaybackState.BUFFERING || pbState == PlaybackState.IDLE)) {
            CircularProgressIndicator()
            return@Row
        }

        IconButton(
            enabled = isConnected, onClick = {
                try {
                    if (currentlyPlaying.value != thisId) {
                        castContext.activeDevice.value?.load(
                            LoadRequest.Url(
                                contentType = type,
                                url = url,
                                volume = castContext.state.volume.value,
                                metadata = Metadata(
                                    title = item.title,
                                    thumbnailUrl = item.itunesItemData?.image,
                                ),
                                resumePosition = null,
                                speed = null,
                                requestHeaders = null,
                            )
                        )
                        currentlyPlaying.value = thisId
                    } else if (castContext.state.playbackState.value == PlaybackState.PLAYING) {
                        castContext.activeDevice.value?.pausePlayback()
                    } else if (castContext.state.playbackState.value == PlaybackState.PAUSED) {
                        castContext.activeDevice.value?.resumePlayback()
                    }
                } catch (_: Throwable) {
                }
            }) {
            Icon(
                imageVector = if (thisIsPlaying && pbState == PlaybackState.PLAYING) Icons.Default.Pause else Icons.Default.PlayArrow,
                contentDescription = if (thisIsPlaying && pbState == PlaybackState.PLAYING) "Pause" else "Play",
                tint = playIconTint,
                modifier = Modifier.size(32.dp)
            )
        }
    }
}

@Composable
fun SimpleDialog(
    onDismissRequest: () -> Unit,
    content: @Composable ColumnScope.() -> Unit,
) {
    Dialog(onDismissRequest) {
        Card(
            modifier = Modifier.fillMaxWidth(),
            shape = CutCornerShape(0.0F),
        ) {
            Column(content = content)
        }
    }
}

@Composable
fun DismissableDialog(
    label: String,
    onDismissRequest: () -> Unit,
    content: @Composable ColumnScope.() -> Unit,
) {
    Dialog(onDismissRequest) {
        Card(
            modifier = Modifier.fillMaxWidth(),
            shape = CutCornerShape(0.0F),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth()
                    .padding(start = 4.dp, end = 4.dp),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically,
            ) {
                Text(label)
                TextButton(onClick = onDismissRequest) {
                    Text("Dismiss")
                }
            }

            Column(content = content)
        }
    }
}

@Composable
fun CastView(castContext: KtCastContext, onDismissRequest: () -> Unit) {
    if (castContext.activeDevice.value == null) {
        DismissableDialog("Cast to", onDismissRequest) {
            Column {
                castContext.devices.value.forEach { deviceInfo ->
                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier
                            .padding(start = 4.dp, end = 4.dp)
                            .height(40.dp)
                            .fillMaxWidth().let {
                                if (!(deviceInfo.addresses.isEmpty() || deviceInfo.port == 0.toUShort())) {
                                    it.clickable {
                                        castContext.connect(deviceInfo)
                                    }
                                } else {
                                    it
                                }
                            },
                    ) {
                        var fgColor = LocalContentColor.current
                        if (deviceInfo.addresses.isEmpty() || deviceInfo.port == 0.toUShort()) {
                            fgColor = fgColor.copy(alpha = 0.5f)
                        }
                        when (deviceInfo.protocol) {
                            ProtocolType.F_CAST -> Icon(
                                painter = painterResource(R.drawable.ic_fc),
                                tint = fgColor,
                                contentDescription = "FCast",
                                modifier = Modifier.size(24.dp)
                            )

                            ProtocolType.CHROMECAST -> Icon(
                                painter = painterResource(R.drawable.ic_chromecast),
                                tint = fgColor,
                                contentDescription = "Chromecast",
                                modifier = Modifier.size(24.dp)
                            )
                        }
                        Text(
                            text = deviceInfo.name,
                            modifier = Modifier.padding(start = 4.dp),
                            fgColor
                        )
                    }
                }

                if (castContext.onQRRequested != null) {
                    TextButton(onClick = {
                        castContext.onQRRequested?.invoke()
                    }) {
                        Text("Scan QR")
                    }
                }
            }
        }
    } else {
        when (castContext.state.connectionState.value) {
            ConnectionState.Connecting -> {
                SimpleDialog(onDismissRequest) {
                    Text("Connecting to ${castContext.activeDevice.value?.name()}")
                }
            }

            ConnectionState.Connected -> {
                DismissableDialog(
                    castContext.activeDevice.value?.name() ?: "n/a",
                    onDismissRequest
                ) {
                    Text("Volume")
                    Slider(
                        value = castContext.state.volume.value.toFloat(),
                        valueRange = 0f..1f,
                        onValueChange = {
                            castContext.state.volume.value = it.toDouble()
                            castContext.setVolume(castContext.state.volume.value)
                        },
                        onValueChangeFinished = {
                            castContext.setVolume(castContext.state.volume.value, force = true)
                        }
                    )

                    TextButton(onClick = {
                        castContext.disconnect()
                    }) {
                        Text("Disconnect")
                    }
                }
            }

            ConnectionState.Reconnecting -> {
                SimpleDialog(onDismissRequest) {
                    Text("Reconnecting to ${castContext.activeDevice.value?.name()}")
                }
            }

            ConnectionState.Disconnected -> {
                SimpleDialog(onDismissRequest) {
                    Text("The device is disconnected so you should not see this.")
                }
            }
        }
    }
}

@Composable
fun CastButton(castContext: KtCastContext) {
    val showingDialog = remember { mutableStateOf(false) }

    IconButton(onClick = {
        showingDialog.value = true
    }) {
        Icon(
            painter = painterResource(R.drawable.ic_cast),
            contentDescription = "Cast",
            Modifier.size(34.dp)
        )
    }
    if (showingDialog.value) {
        CastView(castContext) {
            showingDialog.value = false
        }
    }
}

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun View(
    modifier: Modifier,
    castContext: KtCastContext,
    rssChannel: MutableState<RssChannel?>,
    loadFeed: (String) -> Unit,
) {
    val currentlyPlaying: MutableState<String?> = remember { mutableStateOf(null) }
    var rssUrl by remember { mutableStateOf("https://odysee.com/$/rss/@FUTO:e") }
    val channel = rssChannel.value
    if (channel != null) {
        Scaffold(modifier, topBar = {
            TopAppBar(
                colors = TopAppBarDefaults.topAppBarColors(
                    containerColor = MaterialTheme.colorScheme.primaryContainer,
                    titleContentColor = MaterialTheme.colorScheme.primary,
                ), title = {
                    Text(channel.title ?: "n/a")
                }, actions = {
                    CastButton(castContext)
                    IconButton(onClick = {
                        rssChannel.value = null
                        currentlyPlaying.value = null
                    }) {
                        Icon(imageVector = Icons.Default.Close, contentDescription = "Close")
                    }
                })
        }, bottomBar = {
            if (castContext.activeDevice.value != null) {
                val pbState = castContext.state.playbackState.value
                BottomAppBar {
                    val currentTime = castContext.state.time.value.toFloat()
                    val duration = castContext.state.duration.value.toFloat()
                    Slider(
                        enabled = pbState == PlaybackState.PLAYING || pbState == PlaybackState.PAUSED,
                        value = currentTime,
                        valueRange = 0f..duration,
                        onValueChange = {
                            castContext.state.time.value = it.toDouble()
                        },
                        onValueChangeFinished = {
                            try {
                                castContext.activeDevice.value?.seek(castContext.state.time.value)
                            } catch (e: Throwable) {
                                println("Failed to seek: $e")
                            }
                        })
                }
            }
        }) { innerPadding ->
            LazyColumn(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(innerPadding)
            ) {
                items(channel.items) {
                    FeedItem(currentlyPlaying, castContext, channel.image?.url, it)
                }
            }
        }
    } else {
        Scaffold(modifier) { innerPadding ->
            Column(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(innerPadding),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                TextField(value = rssUrl, onValueChange = {
                    rssUrl = it
                }, label = { Text("RSS feed") })
                Button(onClick = { loadFeed(rssUrl) }) {
                    Text("Load")
                }
            }
        }
    }
}
