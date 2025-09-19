package fcast.sender

import android.os.Bundle
import androidx.activity.ComponentActivity
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.border
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.systemBarsPadding
import androidx.compose.foundation.lazy.LazyColumn
import androidx.compose.foundation.lazy.items
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.filled.Pause
import androidx.compose.material.icons.filled.PlayArrow
import androidx.compose.material3.BottomAppBar
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Scaffold
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.material3.TextField
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.runtime.Composable
import androidx.compose.runtime.MutableState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
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
import org.fcast.sender_sdk.CastButton
import org.fcast.sender_sdk.KtCastContext
import org.fcast.sender_sdk.LoadRequest
import org.fcast.sender_sdk.Metadata
import org.fcast.sender_sdk.PlaybackState

class MainActivity : ComponentActivity() {
    private val castContext = KtCastContext()
    private val barcodeLauncher = registerForActivityResult(ScanContract()) { result ->
        result.contents?.let {
            castContext.QrScanComplete(it)
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
        castContext.onConnected = { }
        castContext.onDisconnected = { }
        enableEdgeToEdge()
        setContent {
            FCastSenderTheme {
                Scaffold(
                    modifier = Modifier
                        .fillMaxSize()
                        .systemBarsPadding()
                ) { innerPadding ->
                    View(
                        Modifier.padding(innerPadding), castContext, rssChannel, { url ->
                            lifecycleScope.launch(Dispatchers.IO) {
                                val channel = rssParser.getRssChannel(url)
                                rssChannel.value = channel
                            }
                        })
                }
            }
        }
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
            castContext.state.connectionState.value == KtCastContext.ConnectionState.Connected
        var playIconTint = MaterialTheme.colorScheme.onBackground
        if (!isConnected) {
            playIconTint = playIconTint.copy(alpha = 0.5f)
        }

        val raw = item.rawEnclosure
        if (raw == null) {
            return@Row
        }
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
                            )
                        )
                        currentlyPlaying.value = thisId
                    } else if (castContext.state.playbackState.value == PlaybackState.PLAYING) {
                        castContext.activeDevice.value?.pausePlayback()
                    } else if (castContext.state.playbackState.value == PlaybackState.PAUSED) {
                        castContext.activeDevice.value?.resumePlayback()
                    }
                } catch (e: Throwable) {
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

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun View(
    modifier: Modifier,
    castContext: KtCastContext,
    rssChannel: MutableState<RssChannel?>,
    loadFeed: (String) -> Unit,
) {
    val currentlyPlaying: MutableState<String?> = remember { mutableStateOf(null) }
    var rssUrl by remember { mutableStateOf("https://podcast.darknetdiaries.com/") }
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
