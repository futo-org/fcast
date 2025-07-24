package fcast.sender

import android.icu.text.DecimalFormat
import android.os.Bundle
import android.view.KeyEvent
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.Card
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableDoubleStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import androidx.compose.ui.window.Dialog
import fcast.sender.ui.theme.FCastSenderTheme
import uniffi.fcast_sender_sdk.DeviceConnectionState
import uniffi.fcast_sender_sdk.CastingDevice
import uniffi.fcast_sender_sdk.DeviceEventHandler
import uniffi.fcast_sender_sdk.GenericKeyEvent
import uniffi.fcast_sender_sdk.GenericMediaEvent
import uniffi.fcast_sender_sdk.PlaybackState
import uniffi.fcast_sender_sdk.Source
import uniffi.fcast_sender_sdk.initLogger
import uniffi.fcast_sender_sdk.IpAddr
import uniffi.fcast_sender_sdk.urlFormatIpAddr
import uniffi.fcast_sender_sdk.deviceInfoFromUrl
import org.fcast.sender_sdk.NsdDeviceDiscoverer
import uniffi.fcast_sender_sdk.CastContext
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import uniffi.fcast_sender_sdk.DeviceInfo
import uniffi.fcast_sender_sdk.DeviceDiscovererEventHandler
import uniffi.fcast_sender_sdk.FileServer

data class CastingState(
    var volume: MutableState<Double> = mutableDoubleStateOf(1.0),
    var playbackState: MutableState<PlaybackState> = mutableStateOf(PlaybackState.IDLE),
    var time: MutableState<Double> = mutableDoubleStateOf(0.0),
    var duration: MutableState<Double> = mutableDoubleStateOf(0.0),
    var speed: MutableState<Double> = mutableDoubleStateOf(1.0),
    var contentType: MutableState<String> = mutableStateOf(""),
    var localAddress: IpAddr? = null,
) {
    fun reset() {
        volume.value = 1.0
        playbackState.value = PlaybackState.IDLE
        time.value = 0.0
        duration.value = 0.0
        speed.value = 1.0
        contentType.value = ""
        localAddress = null
    }
}

class EventHandler : DeviceEventHandler {
    var castingState = CastingState()

    override fun connectionStateChanged(state: DeviceConnectionState) {
        println("Connection state changed: $state")
        when (state) {
            is DeviceConnectionState.Connected -> {
                castingState.localAddress = state.localAddr
            }

            else -> {}
        }
    }

    override fun volumeChanged(volume: Double) {
        println("Volume changed: $volume")
        castingState.volume.value = volume
    }

    override fun timeChanged(time: Double) {
        println("Time changed: $time")
        castingState.time.value = time
    }

    override fun playbackStateChanged(state: PlaybackState) {
        println("Playback state changed: $state")
        castingState.playbackState.value = state
    }

    override fun durationChanged(duration: Double) {
        println("Duration changed: $duration")
        castingState.duration.value = duration
    }

    override fun speedChanged(speed: Double) {
        println("Speed changed: $speed")
        castingState.speed.value = speed
    }

    override fun sourceChanged(source: Source) {
        println("Source changed: $source")
        when (source) {
            is Source.Url -> {
                castingState.contentType.value = source.contentType
            }

            else -> {
                castingState.contentType.value = ""
            }
        }
    }

    override fun keyEvent(event: GenericKeyEvent) {
        // Unreachable
    }

    override fun mediaEvent(event: GenericMediaEvent) {
        // Unreachable
    }
}

class DiscoveryEventHandler(
    private val devices: MutableState<List<CastingDevice>>,
    private val ctx: CastContext
) : DeviceDiscovererEventHandler {
    override fun deviceAvailable(deviceInfo: DeviceInfo) {
        devices.value += ctx.createDeviceFromInfo(deviceInfo)
    }

    override fun deviceChanged(deviceInfo: DeviceInfo) {
        devices.value.find { it.name() == deviceInfo.name }?.let {
            it.setAddresses(deviceInfo.addresses)
            it.setPort(deviceInfo.port)
        }
    }

    override fun deviceRemoved(deviceName: String) {
        devices.value.filter { it.name() != deviceName }.let {
            devices.value = it
        }
    }
}

class MainActivity : ComponentActivity() {
    private val eventHandler = EventHandler()
    private val castContext = CastContext()
    private val fileServer = castContext.startFileServer()
    private var activeCastingDevice: MutableState<CastingDevice?> = mutableStateOf(null)
    private val devices: MutableState<List<CastingDevice>> = mutableStateOf(listOf())
    private val barcodeLauncher = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let {
            deviceInfoFromUrl(it)?.let { deviceInfo ->
                val device = castContext.createDeviceFromInfo(deviceInfo);
                try {
                    device.connect(eventHandler)
                    activeCastingDevice.value = device;
                } catch (e: Exception) {
                    println("Failed to start device: {e}")
                }
            }
        }
    }
    private val selectMediaIntent = registerForActivityResult(ActivityResultContracts.GetContent())
    { maybeUri ->
        try {
            val uri = maybeUri!!
            val type = this.contentResolver.getType(uri)!!
            val parcelFd = this.contentResolver.openFileDescriptor(uri, "r")
            val fd = parcelFd?.detachFd() ?: throw Exception("asdf")
            activeCastingDevice.value?.let { device ->
                val entry = fileServer.serveFile(fd)
                val url =
                    "http://${urlFormatIpAddr(eventHandler.castingState.localAddress!!)}:${entry.port}/${entry.location}"
                device.loadUrl(type, url, null, null)
            }
        } catch (e: Exception) {
            println("Failed to read $maybeUri: $e")
        }
    }
    private lateinit var deviceDiscoverer: NsdDeviceDiscoverer

    init {
        initLogger()
    }

    override fun onKeyDown(keyCode: Int, event: KeyEvent?): Boolean {
        when (keyCode) {
            KeyEvent.KEYCODE_VOLUME_UP -> {
                eventHandler.castingState.volume.value =
                    (eventHandler.castingState.volume.value + 0.1).coerceAtMost(1.0)
                activeCastingDevice.value?.changeVolume(eventHandler.castingState.volume.value)
            }

            KeyEvent.KEYCODE_VOLUME_DOWN -> {
                eventHandler.castingState.volume.value =
                    (eventHandler.castingState.volume.value - 0.1).coerceAtLeast(0.0)
                activeCastingDevice.value?.changeVolume(eventHandler.castingState.volume.value)
            }

            else -> return super.onKeyDown(keyCode, event)
        }
        return true
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        deviceDiscoverer = NsdDeviceDiscoverer(this, DiscoveryEventHandler(devices, castContext))
        enableEdgeToEdge()
        setContent {
            FCastSenderTheme {
                Scaffold(modifier = Modifier.fillMaxSize()) { innerPadding ->
                    View(
                        Modifier.padding(innerPadding),
                        eventHandler.castingState,
                        activeCastingDevice,
                        devices,
                        connectDevice = { device ->
                            try {
                                device.connect(eventHandler)
                                activeCastingDevice.value = device
                            } catch (e: Exception) {
                                println("Failed to connect to device: $e")
                            }
                        },
                        disconnectActiveDevice = {
                            try {
                                activeCastingDevice.value?.disconnect()
                            } catch (e: Exception) {
                                println("Failed to stop device: $e")
                            }
                            activeCastingDevice.value = null
                            eventHandler.castingState.reset()
                        },
                        launchQrScanner = {
                            barcodeLauncher.launch(ScanOptions().setOrientationLocked(false))
                        },
                        selectMedia = {
                            // selectMediaIntent.launch("image/*,video/*,audio/*") // Doesn't show quick select for video and audio, only the first type in the list...
                            selectMediaIntent.launch("*/*")
                        }
                    )
                }
            }
        }
    }
}

@Composable
fun CastDialog(
    onDismissRequest: () -> Unit,
    connectDevice: (CastingDevice) -> Unit,
    devices: MutableState<List<CastingDevice>>,
    launchQrScanner: () -> Unit
) {
    Dialog(onDismissRequest = { onDismissRequest() }) {
        Card(
            modifier = Modifier
                .fillMaxWidth()
                .padding(8.dp),
            shape = RoundedCornerShape(8.dp),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text("Discovered Devices")
                TextButton(onClick = onDismissRequest) {
                    Text("Close")
                }
            }
            Column {
                devices.value.forEach { device ->
                    TextButton(onClick = { connectDevice(device) }) {
                        Text(text = device.name())
                    }
                }
                Button(onClick = launchQrScanner) {
                    Text(text = "Scan QR code")
                }
            }
        }
    }
}

@Composable
fun DeviceDialog(
    onDismissRequest: () -> Unit,
    disconnectActiveDevice: () -> Unit,
    device: CastingDevice,
    state: CastingState
) {
    Dialog(onDismissRequest = { onDismissRequest() }) {
        Card(
            modifier = Modifier
                .fillMaxWidth()
                .padding(8.dp),
            shape = RoundedCornerShape(8.dp),
        ) {
            Row(
                modifier = Modifier
                    .fillMaxWidth(),
                horizontalArrangement = Arrangement.SpaceBetween,
                verticalAlignment = Alignment.CenterVertically
            ) {
                Text("Connected to")
                TextButton(onClick = onDismissRequest) {
                    Text("Close")
                }
            }
            Column {
                Text(text = device.name())
                Text("Volume")
                Slider(
                    value = state.volume.value.toFloat(),
                    onValueChange = {
                        state.volume.value = it.toDouble()
                    },
                    onValueChangeFinished = {
                        try {
                            device.changeVolume(state.volume.value)
                        } catch (e: Exception) {
                            println("Failed to change volume: $e")
                        }
                    }
                )
                Text("Playback speed: ${DecimalFormat("#.##").format(state.speed.value)}x")
                Slider(
                    value = state.speed.value.toFloat(),
                    valueRange = 0.5f..2.0f,
                    onValueChange = {
                        state.speed.value = it.toDouble()
                    },
                    onValueChangeFinished = {
                        try {
                            device.changeSpeed(state.speed.value)
                        } catch (e: Exception) {
                            println("Failed to change playback speed: $e")
                        }
                    }
                )
                Button(onClick = { disconnectActiveDevice() }) {
                    Text("Disconnect")
                }
            }
        }
    }
}

@Composable
fun View(
    modifier: Modifier,
    state: CastingState,
    activeDevice: MutableState<CastingDevice?>,
    devices: MutableState<List<CastingDevice>>,
    connectDevice: (CastingDevice) -> Unit,
    disconnectActiveDevice: () -> Unit,
    launchQrScanner: () -> Unit,
    selectMedia: () -> Unit,
) {
    val openCastDialog = remember { mutableStateOf(false) }

    Column(
        modifier = modifier
            .fillMaxWidth()
            .fillMaxHeight(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center
    ) {
        Button(onClick = {
            openCastDialog.value = true
        }) {
            Text("Devices")
        }
        when (val castingDevice = activeDevice.value) {
            null -> {}
            else -> {
                Button(onClick = {
                    try {
                        castingDevice.loadVideo(
                            "video/mp4",
                            "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                            0.0,
                            1.0
                        )
                    } catch (e: Exception) {
                        println("Failed to load video: $e")
                    }
                }) {
                    Text("Cast demo")
                }
                Button(onClick = selectMedia) {
                    Text("Cast local file")
                }
                if (state.playbackState.value == PlaybackState.PLAYING
                    || state.playbackState.value == PlaybackState.PAUSED
                ) {
                    Button(onClick = {
                        castingDevice.stopPlayback()
                    }) {
                        Text("Stop casting")
                    }
                    if (state.contentType.value.startsWith("video/")) {
                        Text("Scrubber")
                        Slider(
                            value = state.time.value.toFloat(),
                            onValueChange = {
                                state.time.value = it.toDouble()
                            },
                            onValueChangeFinished = {
                                try {
                                    castingDevice.seek(state.time.value)
                                } catch (e: Exception) {
                                    println("Failed to seek: $e")
                                }
                            },
                            valueRange = 0.0f..state.duration.value.toFloat()
                        )
                    }
                }
                if (state.playbackState.value == PlaybackState.PLAYING && state.contentType.value.startsWith(
                        "video/"
                    )
                ) {
                    Button(onClick = {
                        try {
                            castingDevice.pausePlayback()
                        } catch (e: Exception) {
                            println("Failed to pause playback: $e")
                        }
                    }) {
                        Text("Pause")
                    }
                } else if (state.playbackState.value == PlaybackState.PAUSED && state.contentType.value.startsWith(
                        "video/"
                    )
                ) {
                    Button(onClick = {
                        try {
                            castingDevice.resumePlayback()
                        } catch (e: Exception) {
                            println("Failed to resume playback: $e")
                        }
                    }) {
                        Text("Play")
                    }
                } else if (state.playbackState.value == PlaybackState.BUFFERING) {
                    CircularProgressIndicator()
                }
            }
        }
    }
    when {
        openCastDialog.value -> {
            when (val castingDevice = activeDevice.value) {
                null -> {
                    CastDialog(
                        onDismissRequest = { openCastDialog.value = false },
                        connectDevice,
                        devices,
                        launchQrScanner
                    )
                }

                else -> {
                    DeviceDialog(
                        onDismissRequest = { openCastDialog.value = false },
                        disconnectActiveDevice,
                        castingDevice,
                        state
                    )
                }
            }
        }
    }
}
