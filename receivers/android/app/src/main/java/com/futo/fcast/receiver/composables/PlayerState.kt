package com.futo.fcast.receiver.composables

import android.net.Uri
import android.util.Log
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.media3.common.Player
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.DpSize
import androidx.compose.ui.unit.IntSize
import androidx.media3.common.Player.EVENT_IS_PLAYING_CHANGED
import androidx.media3.common.VideoSize
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.PlayerActivity.Companion.TAG
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

@Composable
fun rememberPlayerState(player: Player): PlayerState {
    var currentVideoSize by remember { mutableStateOf<Size?>(null) }

    var currentPosition by remember { mutableLongStateOf(0L) }
    var duration by remember { mutableLongStateOf(0L) }
    var bufferedPosition by remember { mutableLongStateOf(0L) }
    var isPlaying by remember { mutableStateOf(false) }
    var isPlaylist by remember { mutableStateOf(false) }
    var mediaTitle by remember { mutableStateOf<String?>(null) }
    var mediaThumbnail by remember { mutableStateOf<Uri?>(null) }
    var mediaType by remember { mutableStateOf<Int?>(null) }


    val updateState: (events: Player.Events?) -> Unit = {
        if (it?.contains(EVENT_IS_PLAYING_CHANGED) == true && player.isPlaying) {
            PlayerActivity.instance?.mediaPlayHandler()
        }
        else if (player.playbackState == Player.STATE_ENDED) {
            PlayerActivity.instance?.mediaEndHandler()
        }

        currentPosition = player.currentPosition
        duration = player.duration
        bufferedPosition = player.bufferedPosition
        isPlaying = player.isPlaying
        isPlaylist = player.mediaItemCount > 0
        mediaTitle = if (player.mediaMetadata.title.toString() == "null") null else player.mediaMetadata.title.toString()
        mediaThumbnail = player.mediaMetadata.artworkUri
        mediaType = player.mediaMetadata.mediaType
    }

    val scope = rememberCoroutineScope()
    LaunchedEffect(Unit) {
        scope.launch(Dispatchers.Main) {
            while (scope.isActive) {
                try {
                    updateState(null)
                    PlayerActivity.instance?.sendPlaybackUpdate()
                    delay(1000)
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed to send playback update.", e)
                }
            }
        }
    }

    DisposableEffect(player) {
        val listener = object : Player.Listener {
            override fun onEvents(player: Player, events: Player.Events) {
                super.onEvents(player, events)
//                events.contains(EVENT_IS_PLAYING_CHANGED)
                updateState(events)
            }

            override fun onVideoSizeChanged(videoSize: VideoSize) {
//                Log.i("test", "size change ${videoSize.width} ${videoSize.height}")
                currentVideoSize = Size(videoSize.width.toFloat(), videoSize.height.toFloat())
                super.onVideoSizeChanged(videoSize)
            }
        }

        player.addListener(listener)

        onDispose {
            player.removeListener(listener)
        }
    }

    return remember(currentVideoSize, currentPosition, duration, bufferedPosition, isPlaying, isPlaylist, mediaTitle, mediaThumbnail, mediaType) {
        PlayerState(currentVideoSize, currentPosition, duration, bufferedPosition, isPlaying, isPlaylist, mediaTitle, mediaThumbnail, mediaType)
    }
//    return remember(currentPosition, duration, bufferedPosition, isPlaying, isPlaylist, mediaTitle, mediaThumbnail, mediaType) {
//        PlayerState(currentPosition, duration, bufferedPosition, isPlaying, isPlaylist, mediaTitle, mediaThumbnail, mediaType)
//    }
}

data class PlayerState(
    val currentVideoSize: Size?,
    val currentPosition: Long,
    val duration: Long,
    val bufferedPosition: Long,
    val isPlaying: Boolean,
    val isPlaylist: Boolean,
    val mediaTitle: String?,
    val mediaThumbnail: Uri?,
    val mediaType: Int?
)
