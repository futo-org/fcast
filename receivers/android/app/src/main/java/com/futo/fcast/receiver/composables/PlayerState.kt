package com.futo.fcast.receiver.composables

import android.net.Uri
import android.util.Log
import androidx.annotation.OptIn
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.media3.common.Player
import androidx.media3.common.text.Cue
import androidx.media3.common.text.CueGroup
import androidx.media3.common.util.UnstableApi
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.PlayerActivity.Companion.TAG
import com.google.common.collect.ImmutableList
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch

@OptIn(UnstableApi::class)
@Composable
fun rememberPlayerState(player: Player): PlayerState {
    var currentPosition by remember { mutableLongStateOf(0L) }
    var duration by remember { mutableLongStateOf(0L) }
    var bufferedPosition by remember { mutableLongStateOf(0L) }
    var isPlaying by remember { mutableStateOf(false) }
    var isBuffering by remember { mutableStateOf(false) }
    var isPlaylist by remember { mutableStateOf(false) }
    var isLive by remember { mutableStateOf(false) }
    var mediaTitle by remember { mutableStateOf<String?>(null) }
    var mediaThumbnail by remember { mutableStateOf<Uri?>(null) }
    var mediaType by remember { mutableStateOf<Int?>(null) }
    var cues by remember { mutableStateOf<ImmutableList<Cue>?>(null) }

    val updateState: (events: Player.Events?) -> Unit = {
        if (it?.contains(Player.EVENT_IS_PLAYING_CHANGED) == true && player.isPlaying) {
            PlayerActivity.instance?.mediaPlayHandler()
//        } else if (it?.contains(Player.EVENT_MEDIA_ITEM_TRANSITION) == true || player.playbackState == Player.STATE_ENDED) {
//        } else if (it?.contains(Player.EVENT_MEDIA_ITEM_TRANSITION) == true) {
        } else if (player.playbackState == Player.STATE_ENDED) {
            PlayerActivity.instance?.mediaEndHandler()
        }

        currentPosition = player.currentPosition
        duration = player.duration
        bufferedPosition = player.bufferedPosition
        isPlaying = player.isPlaying
        isBuffering = player.playbackState == Player.STATE_BUFFERING
        isPlaylist = player.mediaItemCount > 0
        isLive = player.isCurrentMediaItemLive
        mediaTitle =
            if (player.mediaMetadata.title.toString() == "null") null else player.mediaMetadata.title.toString()
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
                updateState(events)
            }

            override fun onCues(cueGroup: CueGroup) {
                cues = cueGroup.cues
            }
        }

        // todo listener reattach on new exoplayer reference (preload manager rebuilding)
        player.addListener(listener)

        onDispose {
            player.removeListener(listener)
        }
    }

    return remember(
        currentPosition,
        duration,
        bufferedPosition,
        isPlaying,
        isBuffering,
        isPlaylist,
        isLive,
        mediaTitle,
        mediaThumbnail,
        mediaType,
        cues,
    ) {
        PlayerState(
            currentPosition,
            duration,
            bufferedPosition,
            isPlaying,
            isBuffering,
            isPlaylist,
            isLive,
            mediaTitle,
            mediaThumbnail,
            mediaType,
            cues,
        )
    }
}

data class PlayerState(
    val currentPosition: Long,
    val duration: Long,
    val bufferedPosition: Long,
    val isPlaying: Boolean,
    val isBuffering: Boolean,
    val isPlaylist: Boolean,
    val isLive: Boolean,
    val mediaTitle: String?,
    val mediaThumbnail: Uri?,
    val mediaType: Int?,
    val cues: ImmutableList<Cue>?
)
