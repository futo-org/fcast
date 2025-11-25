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
import androidx.core.net.toUri
import androidx.media3.common.C
import androidx.media3.common.MediaMetadata
import androidx.media3.common.PlaybackParameters
import androidx.media3.common.Player
import androidx.media3.common.TrackSelectionParameters
import androidx.media3.common.Tracks
import androidx.media3.common.text.Cue
import androidx.media3.common.text.CueGroup
import androidx.media3.common.util.UnstableApi
import com.futo.fcast.receiver.NetworkService
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.models.GenericMediaMetadata
import com.futo.fcast.receiver.models.VolumeUpdateMessage
import com.google.common.collect.ImmutableList
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.delay
import kotlinx.coroutines.isActive
import kotlinx.coroutines.launch
import java.util.Locale
import java.util.UUID
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
    var subtitles by remember { mutableStateOf(listOf("Off")) }
    var selectedSubtitles by remember { mutableStateOf("Off") }
    var selectedPlaybackSpeed by remember { mutableStateOf("1.00") }

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
        mediaType = player.mediaMetadata.mediaType

        if (it?.contains(Player.EVENT_IS_PLAYING_CHANGED) == true && !isBuffering) {
            isPlaying = player.isPlaying
        }

        val thumbnailUrl =
            (PlayerActivity.instance?.viewModel?.playMessage?.metadata as? GenericMediaMetadata)?.thumbnailUrl
        if (thumbnailUrl != null) {
            mediaThumbnail = thumbnailUrl.toUri()
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

                if (playbackParameters.speed.toString() != selectedPlaybackSpeed) {
                    selectedPlaybackSpeed =
                        String.format(Locale.getDefault(), "%.2f", playbackParameters.speed)
                }
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

            override fun onMediaMetadataChanged(mediaMetadata: MediaMetadata) {
                mediaTitle =
                    if (mediaMetadata.title.toString() == "null") null else mediaMetadata.title.toString()
                mediaThumbnail = mediaMetadata.artworkUri
                val artworkData = mediaMetadata.artworkData

                if ((PlayerActivity.instance?.viewModel?.playMessage?.metadata as? GenericMediaMetadata)?.thumbnailUrl == null && artworkData != null) {
                    mediaThumbnail = PlayerActivity.instance?.saveArtworkDataToUri(
                        artworkData,
                        UUID.randomUUID().toString()
                    )
                }
            }

            override fun onTrackSelectionParametersChanged(parameters: TrackSelectionParameters) {
                super.onTrackSelectionParametersChanged(parameters)

                if (parameters.disabledTrackTypes.contains(C.TRACK_TYPE_TEXT)) {
                    selectedSubtitles = "Off"
                } else {
                    val textOverrides =
                        parameters.overrides.values.filter { it.type == C.TRACK_TYPE_TEXT }

                    if (textOverrides.isNotEmpty()) {
                        val selectedOverride = textOverrides.first()
                        val selectedTrackIndex = selectedOverride.trackIndices.firstOrNull()

                        if (selectedTrackIndex != null) {
                            val trackFormat =
                                selectedOverride.mediaTrackGroup.getFormat(selectedTrackIndex)
                            val languageCode = trackFormat.language ?: "und"
                            val language = PlayerActivity.instance?.getSubtitleString(languageCode)

                            if (language != null && language != "") {
                                selectedSubtitles = language
                            }
                        }
                    }
                }
            }

            override fun onTracksChanged(tracks: Tracks) {
                super.onTracksChanged(tracks)

                val subtitleList = mutableListOf("Off")
                for (trackGroup in tracks.groups) {
                    if (trackGroup.type == C.TRACK_TYPE_TEXT) {
                        for (i in 0 until trackGroup.length) {
                            val trackFormat = trackGroup.getTrackFormat(i)
                            val languageCode = trackFormat.language ?: "und"
                            val language = PlayerActivity.instance?.getSubtitleString(languageCode)

                            if (language != null && language != "") {
                                subtitleList.add(language)
                            }
                        }
                    }
                }

                subtitles = subtitleList
                PlayerActivity.instance?.viewModel?.subtitles = subtitles
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
        subtitles,
        selectedSubtitles,
        selectedPlaybackSpeed,
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
            subtitles,
            selectedSubtitles,
            selectedPlaybackSpeed,
        )
    }
}

data class PlayerState(
    val currentPosition: Long = 0,
    val duration: Long = 0,
    val bufferedPosition: Long = 0,
    val isPlaying: Boolean = false,
    val isBuffering: Boolean = false,
    val isPlaylist: Boolean = false,
    val isLive: Boolean = false,
    val isLiveSeekable: Boolean = false,
    val mediaTitle: String? = null,
    val mediaThumbnail: Uri? = null,
    val mediaType: Int? = null,
    val cues: ImmutableList<Cue>? = null,
    val subtitles: List<String> = listOf("Off"),
    val selectedSubtitles: String = "Off",
    val selectedPlaybackSpeed: String = "1.00",
)

const val TAG = "PlayerState"
