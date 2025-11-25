package com.futo.fcast.receiver.views

import androidx.annotation.OptIn
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.BoxWithConstraints
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.offset
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.requiredWidth
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Slider
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.res.vectorResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.unit.times
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_MIXED
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.compose.state.rememberSeekBackButtonState
import androidx.media3.ui.compose.state.rememberSeekForwardButtonState
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.PlayerState
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.colorLive
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.composables.isAndroidTV
import com.futo.fcast.receiver.models.ControlFocus
import com.futo.fcast.receiver.models.PlayerActivityViewModel
import com.futo.fcast.receiver.models.PlayerSource
import com.futo.fcast.receiver.models.SettingsDialogMenuType
import com.futo.fcast.receiver.models.playbackSpeeds

enum class ButtonType {
    PlayPause,
    PlayNext,
    PlayPrevious,
    Captions,
    Settings,
    SeekForward,
    SeekBackward,
}

@OptIn(UnstableApi::class)
@Composable
fun ControlButton(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier = Modifier,
    buttonType: ButtonType,
    playerState: PlayerState,
) {
    var selected by remember { mutableStateOf(false) }

    val source = viewModel.source

    val (onClick, enabled, toggleShowPrimary) = if (source == null) {
        when (buttonType) {
            ButtonType.PlayPause -> {
                selected = viewModel.controlFocus == ControlFocus.Action
                Triple({}, true, previewShowPlayButton)
            }

            ButtonType.PlayNext -> {
                selected = viewModel.controlFocus == ControlFocus.PlayNext
                Triple({}, true, true)
            }

            ButtonType.PlayPrevious -> {
                selected = viewModel.controlFocus == ControlFocus.PlayPrevious
                Triple({}, true, true)
            }

            ButtonType.Captions -> Triple({}, true, previewShowCaptionsOff)
            ButtonType.Settings -> Triple({}, true, true)
            ButtonType.SeekForward -> {
                selected = viewModel.controlFocus == ControlFocus.SeekForward
                Triple({}, true, true)
            }

            ButtonType.SeekBackward -> {
                selected = viewModel.controlFocus == ControlFocus.SeekBackward
                Triple({}, true, true)
            }
        }
    } else {
        when (buttonType) {
            ButtonType.PlayPause -> {
//                val state = rememberPlayPauseButtonState(viewModel.exoPlayer!!)
                selected = viewModel.controlFocus == ControlFocus.Action
//                Triple(state::onClick, state.isEnabled, state.showPlay)
                Triple({
                    PlayerActivity.instance?.playPauseToggle()
                    Unit
                }, true, !playerState.isPlaying)
            }

            ButtonType.PlayNext -> {
                if (source is PlayerSource.Exo) {
//                val state = rememberNextButtonState(viewModel.exoPlayer!!)
                    selected = viewModel.controlFocus == ControlFocus.PlayNext
//                Triple(state::onClick, state.isEnabled, false)
                    Triple(
                        {
                            PlayerActivity.instance?.setPlaylistItem(source.exoPlayer.nextMediaItemIndex)
                            PlayerActivity.instance?.startUIHideTimer()
                            Unit
                        },
                        source.exoPlayer.currentMediaItemIndex < source.exoPlayer.mediaItemCount - 1,
                        false
                    )
                } else {
                    Triple({}, false, true)
                }
            }

            ButtonType.PlayPrevious -> {
                if (source is PlayerSource.Exo) {
//                val state = rememberPreviousButtonState(viewModel.exoPlayer!!)
                    selected = viewModel.controlFocus == ControlFocus.PlayPrevious
//                Triple(state::onClick, state.isEnabled, false)
                    Triple({
                        PlayerActivity.instance?.setPlaylistItem(source.exoPlayer.previousMediaItemIndex)
                        PlayerActivity.instance?.startUIHideTimer()
                        Unit
                    }, source.exoPlayer.currentMediaItemIndex > 0, false)
                } else {
                    Triple({}, false, true)
                }
            }

            ButtonType.Captions -> Triple({}, false, true)
            ButtonType.Settings -> {
                if (source is PlayerSource.Exo) {
                    selected = viewModel.controlFocus == ControlFocus.Settings
                    Triple(
                        {
                            if (viewModel.showSubtitlesSettingsDialog || viewModel.showPlaybackSpeedSettingsDialog) {
                                viewModel.hideAllSettingDialogs()
                            } else {
                                viewModel.showSettingsDialog = !viewModel.showSettingsDialog
                            }

                            if (!viewModel.showSettingsDialog && !viewModel.showSubtitlesSettingsDialog && !viewModel.showPlaybackSpeedSettingsDialog) {
                                PlayerActivity.instance?.startUIHideTimer()
                            }
                        },
                        true,
                        false
                    )
                } else {
                    Triple({}, false, true)
                }
            }

            ButtonType.SeekForward -> {
                if (source is PlayerSource.Exo) {
                    val state = rememberSeekForwardButtonState(source.exoPlayer)
                    selected = viewModel.controlFocus == ControlFocus.SeekForward
                    Triple({
                        (state::onClick)()
                        PlayerActivity.instance?.startUIHideTimer()
                        Unit
                    }, state.isEnabled, false)
                } else {
                    Triple({}, false, true)
                }
            }

            ButtonType.SeekBackward -> {
                if (source is PlayerSource.Exo) {
                    val state = rememberSeekBackButtonState(source.exoPlayer)
                    selected = viewModel.controlFocus == ControlFocus.SeekBackward
                    Triple({
                        (state::onClick)()
                        PlayerActivity.instance?.startUIHideTimer()
                        Unit
                    }, state.isEnabled, false)
                } else {
                    Triple({}, false, true)
                }
            }
        }
    }

    val (imageVector, contentDescription) = when (buttonType) {
        ButtonType.PlayPause -> Pair(
            if (toggleShowPrimary) ImageVector.vectorResource(R.drawable.ic_play)
            else ImageVector.vectorResource(R.drawable.ic_pause),
            if (toggleShowPrimary) stringResource(R.string.player_button_play)
            else stringResource(R.string.player_button_pause)
        )

        ButtonType.PlayNext -> Pair(
            ImageVector.vectorResource(R.drawable.ic_play_next),
            stringResource(R.string.player_next_button)
        )

        ButtonType.PlayPrevious -> Pair(
            ImageVector.vectorResource(R.drawable.ic_play_previous),
            stringResource(R.string.player_previous_button)
        )

        ButtonType.Captions -> Pair(
            if (toggleShowPrimary) ImageVector.vectorResource(R.drawable.ic_cc_off)
            else ImageVector.vectorResource(R.drawable.ic_cc_on),
            stringResource(R.string.player_captions_button)
        )

        ButtonType.Settings -> Pair(
            ImageVector.vectorResource(R.drawable.ic_settings),
            stringResource(R.string.player_settings_button)
        )

        ButtonType.SeekForward -> Pair(
            ImageVector.vectorResource(R.drawable.ic_seek_forward),
            stringResource(R.string.player_settings_button)
        )

        ButtonType.SeekBackward -> Pair(
            ImageVector.vectorResource(R.drawable.ic_seek_backward),
            stringResource(R.string.player_settings_button)
        )
    }


    val buttonHighlight by animateColorAsState(
        targetValue = if (selected) Color(0x1AFFFFFF) else Color(0x00000000),
        animationSpec = tween(durationMillis = 100)
    )
    Box(
        modifier = Modifier
            .clip(CircleShape)
            .background(buttonHighlight)
    ) {
        IconButton(onClick = onClick, modifier = modifier, enabled = enabled) {
            Icon(
                modifier = modifier,
                imageVector = imageVector,
                contentDescription = contentDescription,
                tint = if (enabled) Color.Unspecified else Color(0x80FFFFFF)
            )
        }
    }
}

@kotlin.OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PlayerProgressBar(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier,
    playerState: PlayerState
) {
    var selected by remember { mutableStateOf(false) }
    selected = viewModel.controlFocus == ControlFocus.ProgressBar

    val duration = playerState.duration.toFloat().coerceAtLeast(0.0f)
    val currentPosition = if (playerState.isLive && !playerState.isLiveSeekable) {
        duration
    } else {
        playerState.currentPosition.toFloat().coerceAtLeast(0.0f).coerceAtMost(duration)
    }
    val bufferedPosition =
        playerState.bufferedPosition.toFloat().coerceAtLeast(0.0f).coerceAtMost(duration)

    BoxWithConstraints(modifier = modifier) {
        val parentWidth = this.maxWidth

        Slider(
            modifier = Modifier.padding(top = 16.dp),
            value = if (duration > 0) currentPosition else 0f,
            onValueChange = {
                val source = viewModel.source
                if (source != null && source is PlayerSource.Exo && (!playerState.isLive || playerState.isLiveSeekable)) {
                    source.exoPlayer.seekTo(it.toLong())
                }
            },
            valueRange = 0f..duration,
            thumb = {
                if (isAndroidTV(LocalContext.current) && selected) {
                    Box(
                        modifier = Modifier
                            .width(2.dp)
                            .height(13.dp)
                            .offset(y = 1.dp)
                            .clip(RoundedCornerShape(50))
                            .background(Color.White)
                    )
                } else if (!isAndroidTV(LocalContext.current)) {
                    Box(
                        modifier = Modifier
                            .size(13.dp)
                            .offset(y = 1.dp)
                            .clip(CircleShape)
                            .background(Color.White)
                    )
                }
            },
            track = {
                val height = if (selected) 6.dp else 4.dp

                Box(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(height)
                        .requiredWidth(parentWidth)
                        .clip(RoundedCornerShape(50))
                        .background(Color(0x4DFFFFFF))
                ) {
                    val bufferWidth = (bufferedPosition / duration) * parentWidth
                    Box(
                        modifier = Modifier
                            .width(bufferWidth)
                            .height(height)
                            .background(Color(0x33FFFFFF))
                    )

                    val progressWidth = (currentPosition / duration) * parentWidth
                    val progressColor = if (playerState.isLive) colorLive else Color(0xFF0A62F5)
                    Box(
                        modifier = Modifier
                            .width(progressWidth)
                            .height(height)
                            .background(progressColor)
                    )
                }
            }
        )
    }
}

@OptIn(UnstableApi::class)
@Composable
fun PlayerControlsAV(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier,
    playerState: PlayerState
) {
    val height = if (playerState.mediaType != MEDIA_TYPE_MIXED) {
        if (playerState.mediaTitle != null) 310.dp else 274.dp
    } else {
        if (playerState.isPlaylist) {
            if (playerState.mediaTitle != null) 150.dp else 114.dp
        } else {
            if (playerState.mediaTitle != null) 80.dp else 0.dp
        }
    }

    Box(modifier, contentAlignment = Alignment.BottomCenter) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(height)
                .background(
//                    brush = Brush.verticalGradient(
//                        colors = listOf(Color(0x00141414), Color(0xFF141414)),
//                    )
                    brush = Brush.verticalGradient(
                        colors = listOf(
                            Color(0x00000000),
                            Color(0xE6000000),
                        ),
                    )
                )
        ) {
            Column(
                verticalArrangement = Arrangement.Bottom,
                modifier = Modifier
                    .fillMaxSize()
                    .padding(horizontal = 32.dp, vertical = 24.dp),
            ) {
                if (playerState.isLive) {
                    Box(
                        modifier = Modifier
                            .padding(bottom = 16.dp)
                            .clip(RoundedCornerShape(4.dp))
                            .background(colorLive)
                    ) {
                        Row(
                            modifier = Modifier.padding(6.dp),
                            verticalAlignment = Alignment.CenterVertically,
                            horizontalArrangement = Arrangement.spacedBy(2.dp)
                        ) {
                            Image(
                                painter = painterResource(R.drawable.ic_live),
                                contentDescription = null,
                                modifier = Modifier.size(12.dp)
                            )
                            ThemedText(
                                stringResource(R.string.live_badge),
                                fontSize = 10.sp,
                                fontWeight = FontWeight.SemiBold
                            )
                        }
                    }
                }
                if (playerState.mediaTitle != null) {
                    Text(
                        text = playerState.mediaTitle,
                        modifier = Modifier.fillMaxWidth(0.5f),
                        color = Color.White,
                        fontSize = 18.sp,
                        fontFamily = interFontFamily,
                        fontWeight = FontWeight.Normal,
                        maxLines = 2,
                        overflow = TextOverflow.Ellipsis
                    )
                }

                if (previewControls || viewModel.source is PlayerSource.Exo && (viewModel.hasDuration || playerState.isPlaylist)) {
                    if ((!playerState.isLive || playerState.isLiveSeekable) && playerState.mediaType != MEDIA_TYPE_MIXED) {
                        Spacer(
                            modifier = Modifier
                                .fillMaxWidth()
                                .height(16.dp)
                        )

                        Row(
                            horizontalArrangement = Arrangement.SpaceBetween,
                            verticalAlignment = Alignment.Bottom,
                            modifier = Modifier
                                .fillMaxWidth()
                                .padding(vertical = 6.dp)
                        ) {
                            ThemedText(
                                PlayerActivity.formatDuration(playerState.currentPosition),
                                fontSize = 12.sp
                            )
                            ThemedText(
                                PlayerActivity.formatDuration(playerState.duration),
                                fontSize = 12.sp
                            )
                        }
                    }

                    if (playerState.mediaType != MEDIA_TYPE_MIXED) {
                        PlayerProgressBar(
                            viewModel,
                            modifier = Modifier
                                .fillMaxWidth()
                                .height(20.dp),
                            playerState
                        )
                    }

                    Row(
                        horizontalArrangement = Arrangement.SpaceBetween,
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(top = 12.dp)
                    ) {
                        Spacer(
                            modifier = Modifier
                                .size(44.dp)
                                .padding(6.dp)
                        )
                        Spacer(modifier = Modifier.weight(1f))

                        if (playerState.isPlaylist) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.PlayPrevious,
                                playerState
                            )
                        } else {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.SeekBackward,
                                playerState
                            )
                        }

                        Spacer(modifier = Modifier.width(36.dp))
                        if (playerState.mediaType != MEDIA_TYPE_MIXED || viewModel.hasDuration) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(56.dp)
                                    .padding(6.dp),
                                ButtonType.PlayPause,
                                playerState
                            )
                        }

                        Spacer(modifier = Modifier.width(36.dp))
                        if (playerState.isPlaylist) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.PlayNext,
                                playerState
                            )
                        } else {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.SeekForward,
                                playerState
                            )
                        }

                        Spacer(modifier = Modifier.weight(1f))
                        if (playerState.mediaType != MEDIA_TYPE_MIXED) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.Settings,
                                playerState
                            )
                        }
                    }
                }
            }

            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(height),
                contentAlignment = Alignment.BottomEnd
            ) {
                val dialogModifier = Modifier
                    .width(250.dp)
                    .offset((-15).dp, (-70).dp)
                    .padding(16.dp)

                if (viewModel.showSettingsDialog) {
                    SettingsDialog(
                        viewModel,
                        modifier = dialogModifier,
                        playerState
                    )
                }

                if (viewModel.showSubtitlesSettingsDialog) {
                    ListItemSettingsDialog(
                        viewModel,
                        modifier = dialogModifier,
                        playerState,
                        SettingsDialogMenuType.Subtitles,
                        stringResource(R.string.captions),
                        playerState.subtitles
                    ) {
                        viewModel.showSettingsDialog()
                    }
                }

                if (viewModel.showPlaybackSpeedSettingsDialog) {
                    ListItemSettingsDialog(
                        viewModel,
                        modifier = dialogModifier,
                        playerState,
                        SettingsDialogMenuType.PlaybackSpeed,
                        stringResource(R.string.playback_speed),
                        playbackSpeeds
                    ) {
                        viewModel.showSettingsDialog()
                    }
                }
            }
        }
    }
}

var previewControls = false

const val previewShowPlayButton = true
const val previewShowCaptionsOff = true

@OptIn(UnstableApi::class)
@Preview
@Composable
fun PlayerControlsPreview() {
    previewControls = true
    val viewModel = PlayerActivityViewModel()
//    viewModel.controlFocus = ControlFocus.ProgressBar
    viewModel.controlFocus = ControlFocus.Action
//    viewModel.showSettingsDialog = true
//    viewModel.showPlaybackSpeedSettingsDialog = true

    val playerState = PlayerState(
        1000L * 30,
        1000L * 60,
        1000L * 45,
        true,
        isBuffering = false,
        isPlaylist = false,
        isLive = false,
        isLiveSeekable = false,
//        null,
        "Video Title",
//        "Lorem ipsum dolor sit amet consectetur adipiscing elit. Consectetur adipiscing",
        null,
        1,
        null,
    )

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Gray)
    ) {
        PlayerControlsAV(viewModel, modifier = Modifier.fillMaxSize(), playerState)
    }
}

@OptIn(UnstableApi::class)
@Preview
@Composable
fun PlayerControlsLivePreview() {
    previewControls = true
    val viewModel = PlayerActivityViewModel()
//    viewModel.controlFocus = ControlFocus.ProgressBar
    viewModel.controlFocus = ControlFocus.Action

    val playerState = PlayerState(
        1000L * 30,
        1000L * 60,
        1000L * 45,
        true,
        isBuffering = false,
        isPlaylist = false,
        isLive = true,
        isLiveSeekable = false,
//        null,
        "Video Title",
//        "Lorem ipsum dolor sit amet consectetur adipiscing elit. Consectetur adipiscing",
        null,
        1,
        null,
    )

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Gray)
    ) {
        PlayerControlsAV(viewModel, modifier = Modifier.fillMaxSize(), playerState)
    }
}

@OptIn(UnstableApi::class)
@Preview
@Composable
fun PlayerControlsImagePreview() {
    previewControls = true
    val viewModel = PlayerActivityViewModel()
    viewModel.controlFocus = ControlFocus.Action
//    viewModel.hasDuration = false
    viewModel.hasDuration = true

    val playerState = PlayerState(
        1000L * 30,
        1000L * 60,
        1000L * 45,
        true,
        isBuffering = false,
        isPlaylist = true,
        isLive = false,
        isLiveSeekable = false,
//        null,
        "Video Title",
//        "Lorem ipsum dolor sit amet consectetur adipiscing elit. Consectetur adipiscing",
        null,
        0,
        null,
    )

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Gray)
    ) {
        PlayerControlsAV(viewModel, modifier = Modifier.fillMaxSize(), playerState)
    }
}
