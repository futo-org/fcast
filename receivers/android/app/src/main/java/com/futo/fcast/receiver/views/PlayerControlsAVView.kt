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
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.res.vectorResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.unit.times
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.compose.state.rememberNextButtonState
import androidx.media3.ui.compose.state.rememberPlayPauseButtonState
import androidx.media3.ui.compose.state.rememberPreviousButtonState
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.PlayerState
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.colorLive
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.models.ControlFocus
import com.futo.fcast.receiver.models.PlayerActivityViewModel

enum class ButtonType {
    PlayPause,
    PlayNext,
    PlayPrevious,
    Captions,
    Settings
}

@OptIn(UnstableApi::class)
@Composable
fun ControlButton(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier = Modifier,
    buttonType: ButtonType,
    exoPlayer: Player? = null
) {
    var selected by remember { mutableStateOf(false) }

    val (onClick, enabled, toggleShowPrimary) = if (exoPlayer == null) {
        when (buttonType) {
            ButtonType.PlayPause -> {
                selected = viewModel.controlFocus == ControlFocus.Action
                Triple({}, true, previewShowPlayButton)
            }

            ButtonType.PlayNext -> {
                selected = viewModel.controlFocus == ControlFocus.PlayNext
                Triple({
                    PlayerActivity.instance?.nextPlaylistItem()
                    Unit
                }, true, true)
            }

            ButtonType.PlayPrevious -> {
                selected = viewModel.controlFocus == ControlFocus.PlayPrevious
                Triple({
                    PlayerActivity.instance?.previousPlaylistItem()
                    Unit
                }, true, true)
            }

            ButtonType.Captions -> Triple({}, true, previewShowCaptionsOff)
            ButtonType.Settings -> Triple({}, true, true)
        }
    } else {
        when (buttonType) {
            ButtonType.PlayPause -> {
                val state = rememberPlayPauseButtonState(exoPlayer)
                selected = viewModel.controlFocus == ControlFocus.Action
                Triple(state::onClick, state.isEnabled, state.showPlay)
            }

            ButtonType.PlayNext -> {
                val state = rememberNextButtonState(exoPlayer)
                selected = viewModel.controlFocus == ControlFocus.PlayNext
                Triple(state::onClick, state.isEnabled, false)
            }

            ButtonType.PlayPrevious -> {
                val state = rememberPreviousButtonState(exoPlayer)
                selected = viewModel.controlFocus == ControlFocus.PlayPrevious
                Triple(state::onClick, state.isEnabled, false)
            }

            ButtonType.Captions -> Triple({}, false, true)
            ButtonType.Settings -> Triple({}, false, true)
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
                tint = Color.Unspecified
            )
        }
    }
}

@kotlin.OptIn(ExperimentalMaterial3Api::class)
@Composable
fun PlayerProgressBar(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier,
    exoPlayer: Player? = null,
    playerState: PlayerState
) {
    var selected by remember { mutableStateOf(false) }
    selected = viewModel.controlFocus == ControlFocus.ProgressBar

    val duration = playerState.duration.toFloat().coerceAtLeast(0.0f)
    val currentPosition =
        playerState.currentPosition.toFloat().coerceAtLeast(0.0f).coerceAtMost(duration)
    val bufferedPosition =
        playerState.bufferedPosition.toFloat().coerceAtLeast(0.0f).coerceAtMost(duration)

    BoxWithConstraints(modifier = modifier) {
        val parentWidth = this.maxWidth

        Slider(
            modifier = Modifier.padding(top = 16.dp),
            value = if (duration > 0) currentPosition else 0f,
            onValueChange =
                if (exoPlayer != null) {
                    {
                        exoPlayer.seekTo(it.toLong())
                    }
                } else {
                    {}
                },
            valueRange = 0f..duration,
            thumb = {
                if (selected) {
                    Box(
                        modifier = Modifier
                            .size(13.dp)
                            .offset(y = 1.dp)
                            .clip(CircleShape)
                            .background(Color.White)
                    )
                } else {
                    Box(
                        modifier = Modifier
                            .width(1.dp)
                            .height(13.dp)
                            .offset(y = 1.dp)
                            .clip(RoundedCornerShape(50))
                            .background(Color.White)
                    )
                }
            },
            track = {
                Box(
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(4.dp)
                        .requiredWidth(parentWidth)
                        .clip(RoundedCornerShape(50))
                        .background(Color(0x4DFFFFFF))
                ) {
                    val bufferWidth = (bufferedPosition / duration) * parentWidth
                    Box(
                        modifier = Modifier
                            .width(bufferWidth)
                            .height(4.dp)
                            .background(Color(0x33FFFFFF))
                    )

                    val progressWidth = (currentPosition / duration) * parentWidth
                    val progressColor = if (playerState.isLive) colorLive else Color(0xFF0A62F5)
                    Box(
                        modifier = Modifier
                            .width(progressWidth)
                            .height(4.dp)
                            .background(progressColor)
                    )
                }
            }
        )
    }
}

@Composable
fun PlayerControlsAV(
    viewModel: PlayerActivityViewModel,
    modifier: Modifier,
    exoPlayer: Player? = null,
    playerState: PlayerState
) {
    val height = if (playerState.mediaTitle != null) 310.dp else 274.dp

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
                PlayerProgressBar(
                    viewModel,
                    modifier = Modifier
                        .fillMaxWidth()
                        .height(20.dp),
                    exoPlayer,
                    playerState
                )
                Row(
                    horizontalArrangement = Arrangement.SpaceBetween,
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(top = 12.dp)
                ) {
                    Row(
                        horizontalArrangement = Arrangement.Start,
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier
                            .width(32.dp)
                    ) {
                        if (controlPlayerSettingsShow) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier.size(20.dp),
                                ButtonType.Captions,
                                exoPlayer
                            )
                        }
                    }

                    Row(
                        verticalAlignment = Alignment.CenterVertically,
                        horizontalArrangement = Arrangement.spacedBy(36.dp),
                        modifier = Modifier
                    ) {
                        if (playerState.isPlaylist) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.PlayPrevious,
                                exoPlayer
                            )
                        }
                        ControlButton(
                            viewModel,
                            modifier = Modifier
                                .size(56.dp)
                                .padding(6.dp),
                            ButtonType.PlayPause,
                            exoPlayer
                        )
                        if (playerState.isPlaylist) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier
                                    .size(44.dp)
                                    .padding(6.dp),
                                ButtonType.PlayNext,
                                exoPlayer
                            )
                        }
                    }

                    Row(
                        horizontalArrangement = Arrangement.End,
                        verticalAlignment = Alignment.CenterVertically,
                        modifier = Modifier
                            .width(32.dp)
                    ) {
                        if (controlPlayerSettingsShow) {
                            ControlButton(
                                viewModel,
                                modifier = Modifier.size(20.dp),
                                ButtonType.Settings,
                                exoPlayer
                            )
                        }
                    }
                }
            }
        }
    }
}

// TODO: Refractor UI when placement is decided on
var controlPlayerSettingsShow = false

const val previewShowPlayButton = true
const val previewShowCaptionsOff = true

@OptIn(UnstableApi::class)
@Preview
@Composable
fun PlayerControlsAVPreview() {
    val viewModel = PlayerActivityViewModel()
//    viewModel.controlFocus = ControlFocus.ProgressBar
    viewModel.controlFocus = ControlFocus.Action

    val playerState = PlayerState(
        null,
        1000L * 30,
        1000L * 60,
        1000L * 45,
        true,
        true,
        isLive = true,
//        null,
        "Video Title",
//        "Lorem ipsum dolor sit amet consectetur adipiscing elit. Consectetur adipiscing",
        null,
        0
    )
//    controlPlayerSettingsShow = true

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Gray)
    ) {
        PlayerControlsAV(viewModel, modifier = Modifier.fillMaxSize(), null, playerState)
    }
}
