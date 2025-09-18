package fcast.sender

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.mutableStateOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import com.journeyapps.barcodescanner.ScanContract
import com.journeyapps.barcodescanner.ScanOptions
import fcast.sender.ui.theme.FCastSenderTheme
import org.fcast.sender_sdk.CastButton
import org.fcast.sender_sdk.KtCastContext
import org.fcast.sender_sdk.PlaybackState

class MainActivity : ComponentActivity() {
    private var isConnected = mutableStateOf(false)
    private val castContext = KtCastContext()
    private val barcodeLauncher = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let {
            castContext.QrScanComplete(it)
        }
    }
    private val selectMediaIntent =
        registerForActivityResult(ActivityResultContracts.GetContent()) { maybeUri ->
            try {
                val uri = maybeUri!!
                val type = this.contentResolver.getType(uri)!!
                val parcelFd = this.contentResolver.openFileDescriptor(uri, "r")
                val fd = parcelFd?.detachFd() ?: throw Exception("Failed to detatch fd")
                castContext.loadFileDescriptor(fd, type)
            } catch (e: Throwable) {
                println("Failed to read $maybeUri: $e")
            }
        }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        castContext.startDiscovery(this)
        castContext.onQRRequested = {
            barcodeLauncher.launch(ScanOptions().setOrientationLocked(false))
        }
        castContext.onConnected = {
            isConnected.value = true
        }
        castContext.onDisconnected = {
            isConnected.value = false
        }
        enableEdgeToEdge()
        setContent {
            FCastSenderTheme {
                Scaffold(modifier = Modifier.fillMaxSize()) { innerPadding ->
                    View(
                        Modifier.padding(innerPadding), isConnected, castContext, selectMedia = {
                            // selectMediaIntent.launch("image/*,video/*,audio/*") // Doesn't show quick select for video and audio, only the first type in the list...
                            selectMediaIntent.launch("*/*")
                        })
                }
            }
        }
    }
}

@Composable
fun View(
    modifier: Modifier,
    isConnected: MutableState<Boolean>,
    castContext: KtCastContext,
    selectMedia: () -> Unit,
) {
    Column(
        modifier = modifier
            .fillMaxWidth()
            .fillMaxHeight(),
        horizontalAlignment = Alignment.CenterHorizontally,
        verticalArrangement = Arrangement.Center
    ) {
        CastButton(castContext)

        if (isConnected.value) {
            Button(onClick = {
                castContext.loadUrl(
                    "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4",
                    "video/mp4",
                )
            }) {
                Text("Cast demo")
            }

            Button(onClick = selectMedia) {
                Text("Cast local file")
            }
        }
        when (val castingDevice = castContext.activeDevice.value) {
            null -> {}
            else -> {
                val playbackState = castContext.state.playbackState.value
                val contentType = castContext.state.contentType.value
                if (playbackState == PlaybackState.PLAYING || playbackState == PlaybackState.PAUSED) {
                    Button(onClick = {
                        castingDevice.stopPlayback()
                    }) {
                        Text("Stop casting")
                    }
                    if (castContext.state.contentType.value.startsWith("video/")) {
                        Text("Scrubber")
                        Slider(
                            value = castContext.state.time.value.toFloat(), onValueChange = {
                            castContext.state.time.value = it.toDouble()
                        }, onValueChangeFinished = {
                            try {
                                castingDevice.seek(castContext.state.time.value)
                            } catch (e: Exception) {
                                println("Failed to seek: $e")
                            }
                        }, valueRange = 0.0f..castContext.state.duration.value.toFloat()
                        )
                    }
                }
                if (playbackState == PlaybackState.PLAYING && contentType.startsWith(
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
                } else if (playbackState == PlaybackState.PAUSED && contentType.startsWith(
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
                } else if (playbackState == PlaybackState.BUFFERING) {
                    CircularProgressIndicator()
                }
            }
        }
    }
}
