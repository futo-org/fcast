package com.futo.fcast.receiver.composables

import android.net.Uri
import android.util.Log
import androidx.annotation.OptIn
import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.Player
import androidx.media3.common.text.Cue
import androidx.media3.common.text.CueGroup
import androidx.media3.common.util.UnstableApi
import com.futo.fcast.receiver.NetworkService
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.models.VolumeUpdateMessage
import com.google.common.collect.ImmutableList
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import kotlin.math.abs

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
    var isLiveSeekable by remember { mutableStateOf(false) }
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
        isBuffering = player.playbackState == Player.STATE_BUFFERING
        isPlaylist = player.mediaItemCount > 1
        isLive = player.isCurrentMediaItemLive
        isLiveSeekable = isLive && duration > 60_000
        mediaTitle =
            if (player.mediaMetadata.title.toString() == "null") null else player.mediaMetadata.title.toString()
        mediaThumbnail = player.mediaMetadata.artworkUri
        mediaType = player.mediaMetadata.mediaType

        if (it?.contains(Player.EVENT_IS_PLAYING_CHANGED) == true && !isBuffering) {
            isPlaying = player.isPlaying
        }
    }

    val scope = rememberCoroutineScope()

    DisposableEffect(player) {
        val listener = object : Player.Listener {
            override fun onEvents(player: Player, events: Player.Events) {
                super.onEvents(player, events)
                updateState(events)
            }

            override fun onCues(cueGroup: CueGroup) {
                cues = cueGroup.cues
            }

            override fun onPlayWhenReadyChanged(playWhenReady: Boolean, reason: Int) {
                PlayerActivity.instance?.sendPlaybackUpdate()
                PlayerActivity.instance?.updateKeepScreenOnFlag()
            }

            override fun onPositionDiscontinuity(
                oldPosition: Player.PositionInfo,
                newPosition: Player.PositionInfo,
                reason: Int
            ) {
                updateState(null)
                PlayerActivity.instance?.sendPlaybackUpdate()
            }

            override fun onPlaybackParametersChanged(playbackParameters: PlaybackParameters) {
                PlayerActivity.instance?.sendPlaybackUpdate()
            }

            override fun onVolumeChanged(volume: Float) {
                super.onVolumeChanged(volume)
                scope.launch(Dispatchers.IO) {
                    try {
                        NetworkService.instance?.sendVolumeUpdate(
                            VolumeUpdateMessage(
                                System.currentTimeMillis(),
                                volume.toDouble()
                            )
                        )
                    } catch (e: Throwable) {
                        Log.e(TAG, "Unhandled error sending volume update", e)
                    }
                }
            }

//            override fun onTimelineChanged(timeline: Timeline, reason: Int) {
//                if (isLive && !timeline.isEmpty) {
//                    val window = timeline.getWindow(player.currentMediaItemIndex, Timeline.Window())
//                    isLiveSeekable = window.isSeekable
//                }
//            }
        }
        player.addListener(listener)

        val playbackUpdateIntervalMs = 1000L
        var lastUpdateTime = System.currentTimeMillis()
        var cancelUpdateLoop = false
        scope.launch(Dispatchers.Main) {
            while (scope.isActive && !cancelUpdateLoop) {
                try {
                    val now = System.currentTimeMillis()
                    val delayTime = if (abs(now - lastUpdateTime) > playbackUpdateIntervalMs) {
                        updateState(null)
                        PlayerActivity.instance?.sendPlaybackUpdate()
                        lastUpdateTime = now
                        playbackUpdateIntervalMs
                    } else {
                        abs(now - lastUpdateTime)
                    }

                    delay(delayTime)
                } catch (e: Throwable) {
                    Log.e(TAG, "Failed to send playback update.", e)
                }
            }
        }

        onDispose {
            cancelUpdateLoop = true
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
        isLiveSeekable,
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
            isLiveSeekable,
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
    val isLiveSeekable: Boolean,
    val mediaTitle: String?,
    val mediaThumbnail: Uri?,
    val mediaType: Int?,
    val cues: ImmutableList<Cue>?
)

const val TAG = "PlayerState"
