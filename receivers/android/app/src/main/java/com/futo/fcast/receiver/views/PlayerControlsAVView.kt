package com.futo.fcast.receiver.views

import android.view.Gravity
import androidx.annotation.OptIn
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.layout.wrapContentSize
import androidx.compose.foundation.layout.wrapContentWidth
import androidx.compose.foundation.text.BasicText
//import androidx.compose.material.Icon
//import androidx.compose.material.IconButton
//import androidx.compose.material.Text
//import androidx.compose.material.TextButton
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.Slider
import androidx.compose.runtime.Composable
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.platform.LocalView
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.res.vectorResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.window.Dialog
import androidx.compose.ui.window.DialogProperties
import androidx.compose.ui.window.DialogWindowProvider
import androidx.constraintlayout.compose.ConstraintLayout
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.compose.state.rememberNextButtonState
import androidx.media3.ui.compose.state.rememberPlayPauseButtonState
import androidx.media3.ui.compose.state.rememberPlaybackSpeedState
import androidx.media3.ui.compose.state.rememberPreviousButtonState
import com.futo.fcast.receiver.R

import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.composables.rememberPlayerState


@OptIn(UnstableApi::class)
@Composable
fun PlayPauseButton(modifier: Modifier = Modifier, exoPlayer: Player? = null) {
    if (exoPlayer != null) {
        val state = rememberPlayPauseButtonState(exoPlayer)

        IconButton(onClick = state::onClick, modifier = modifier, enabled = state.isEnabled) {
            Icon(
                imageVector =
                    if (state.showPlay) ImageVector.vectorResource(R.drawable.ic_play)
                    else ImageVector.vectorResource(R.drawable.ic_pause),
                contentDescription =
                    if (state.showPlay) stringResource(R.string.player_button_play)
                    else stringResource(R.string.player_button_pause),
                tint = Color.Unspecified
            )
        }
    }
    else {
        IconButton(onClick = {}, modifier = modifier, enabled = true) {
            Icon(
                imageVector = ImageVector.vectorResource(R.drawable.ic_play),
                contentDescription = null,
                tint = Color.Unspecified
            )
        }
    }
}

@OptIn(UnstableApi::class)
@Composable
fun NextButton(modifier: Modifier = Modifier, exoPlayer: Player? = null) {
    if (exoPlayer != null) {
        val state = rememberNextButtonState(exoPlayer)

        IconButton(onClick = state::onClick, modifier = modifier, enabled = state.isEnabled) {
            Icon(
                imageVector = ImageVector.vectorResource(R.drawable.ic_play_next),
                contentDescription = stringResource(R.string.player_next_button),
                tint = Color.Unspecified
            )
        }
    }
    else {
        IconButton(onClick = {}, modifier = modifier, enabled = true) {
            Icon(
                imageVector = ImageVector.vectorResource(R.drawable.ic_play_next),
                contentDescription = stringResource(R.string.player_next_button),
                tint = Color.Unspecified
            )
        }
    }
}

@OptIn(UnstableApi::class)
@Composable
fun PreviousButton(modifier: Modifier = Modifier, exoPlayer: Player? = null) {
    if (exoPlayer != null) {
        val state = rememberPreviousButtonState(exoPlayer)

        IconButton(onClick = state::onClick, modifier = modifier, enabled = state.isEnabled) {
            Icon(
                imageVector = ImageVector.vectorResource(R.drawable.ic_play_previous),
                contentDescription = stringResource(R.string.player_previous_button),
                tint = Color.Unspecified
            )
        }
    }
    else {
        IconButton(onClick = {}, modifier = modifier, enabled = true) {
            Icon(
                imageVector = ImageVector.vectorResource(R.drawable.ic_play_previous),
                contentDescription = stringResource(R.string.player_previous_button),
                tint = Color.Unspecified
            )
        }
    }
}

@OptIn(UnstableApi::class)
@Composable
fun PlaybackSpeedPopUpButton(modifier: Modifier = Modifier, exoPlayer: Player? = null,
    speedSelection: List<Float> = listOf(0.5f, 0.75f, 1.0f, 1.25f, 1.5f, 1.75f, 2.0f),
) {
    if (exoPlayer != null) {
        val state = rememberPlaybackSpeedState(exoPlayer)
        var openDialog by remember { mutableStateOf(false) }
        TextButton(onClick = { openDialog = true }, modifier = modifier, enabled = state.isEnabled) {
            // TODO: look into TextMeasurer to ensure 1.1 and 2.2 occupy the same space
            BasicText("%.1fx".format(state.playbackSpeed))
        }

//        IconButton(onClick = state::onClick, modifier = modifier, enabled = state.isEnabled) {
//            Icon(
//                imageVector = ImageVector.vectorResource(R.drawable.ic_play_previous),
//                contentDescription = stringResource(R.string.player_previous_button),
//                tint = Color.Unspecified
//            )
//        }

        //                            Image(painter = painterResource(R.drawable.ic_settings),
//                                contentDescription = null,
//                                modifier = Modifier.size(20.dp))
        if (openDialog) {
            BottomDialogOfChoices(
                currentSpeed = state.playbackSpeed,
                choices = speedSelection,
                onDismissRequest = { openDialog = false },
                onSelectChoice = state::updatePlaybackSpeed,
            )
        }
    }
    else {
        var openDialog by remember { mutableStateOf(false) }
        TextButton(onClick = { openDialog = true }, modifier = modifier, enabled = true) {
            // TODO: look into TextMeasurer to ensure 1.1 and 2.2 occupy the same space
            BasicText("%.1fx".format(1.0f))
        }
        if (openDialog) {
            BottomDialogOfChoices(
                currentSpeed = 1.0f,
                choices = speedSelection,
                onDismissRequest = { openDialog = false },
                onSelectChoice = {},
            )
        }
    }

}

@Composable
fun BottomDialogOfChoices(
    currentSpeed: Float,
    choices: List<Float>,
    onDismissRequest: () -> Unit,
    onSelectChoice: (Float) -> Unit,
) {
    Dialog(
        onDismissRequest = onDismissRequest,
        properties = DialogProperties(usePlatformDefaultWidth = false),
    ) {
        val dialogWindowProvider = LocalView.current.parent as? DialogWindowProvider
        dialogWindowProvider?.window?.let { window ->
            window.setGravity(Gravity.BOTTOM) // Move down, by default dialogs are in the centre
            window.setDimAmount(0f) // Remove dimmed background of ongoing playback
        }

        Box(modifier = Modifier.wrapContentSize().background(Color.LightGray)) {
            Column(
                modifier = Modifier.fillMaxWidth().wrapContentWidth(),
                verticalArrangement = Arrangement.Center,
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                choices.forEach { speed ->
                    TextButton(
                        onClick = {
                            onSelectChoice(speed)
                            onDismissRequest()
                        }
                    ) {
                        var fontWeight = FontWeight(400)
                        if (speed == currentSpeed) {
                            fontWeight = FontWeight(1000)
                        }
                        Text("%.1fx".format(speed), fontWeight = fontWeight)
                    }
                }
            }
        }
    }
}

@Composable
fun PlayerProgressBar(modifier: Modifier, exoPlayer: Player? = null) {
    if (exoPlayer != null) {
        val playerState = rememberPlayerState(exoPlayer)
        val currentPosition = playerState.currentPosition.toFloat()
        val duration = playerState.duration.toFloat()

        Slider(
            value = if (duration > 0) currentPosition else 0f,
            onValueChange = {
                // Seek to the new position
                exoPlayer.seekTo(it.toLong())
            },
            valueRange = 0f..duration,
            modifier = Modifier.fillMaxWidth()
        )

//    Slider(
//
//    )


    }
    else {
        Box {
            Box(
                modifier = Modifier
                    .fillMaxWidth()
                    .height(4.dp)
                    .background(Color(0x4DFFFFFF))
            )
            Box(
                modifier = Modifier
                    .width(200.dp)
                    .height(4.dp)
                    .background(Color(0x33FFFFFF))
            )
            Box(
                modifier = Modifier
                    .width(160.dp)
                    .height(4.dp)
                    .background(Color(0xFF0A62F5))
            )
        }
    }
}

@Composable
fun PlayerControlsAV(modifier: Modifier, exoPlayer: Player? = null) {
    Box(modifier, contentAlignment = Alignment.BottomCenter) {
        Box(
            modifier = Modifier
                .fillMaxWidth()
                .height(180.dp)
                .background(
                    brush = Brush.verticalGradient(
                        colors = listOf(Color(0x00141414), Color(0xFF141414)),
                    )
                )
        ) {
            ConstraintLayout(
                modifier = Modifier
                    .fillMaxSize()
                    .padding(horizontal = 32.dp, vertical = 24.dp)
            ) {
                Column(
                    verticalArrangement = Arrangement.Bottom,
                    modifier = Modifier
                        .fillMaxSize(),
                ) {
                    Text(
                        text = "Video Title",
                        modifier = Modifier
                            .fillMaxWidth(),
                        color = Color.White,
                        fontSize = 18.sp,
                        fontFamily = interFontFamily,
                        fontWeight = FontWeight.Normal,
                    )
                    Row(
                        horizontalArrangement = Arrangement.SpaceBetween,
                        modifier = Modifier
                            .fillMaxWidth()
                            .padding(top = 20.dp, bottom = 10.dp)
                    ) {
                        Text(
                            text = "00:00",
                            modifier = Modifier,
                            color = Color.White,
                            fontSize = 12.sp,
                            fontFamily = interFontFamily,
                            fontWeight = FontWeight.Normal,
                        )

                        Text(
                            text = "00:00",
                            modifier = Modifier,
                            color = Color.White,
                            fontSize = 12.sp,
                            fontFamily = interFontFamily,
                            fontWeight = FontWeight.Normal,
                        )
                    }
                    PlayerProgressBar(Modifier.fillMaxWidth(), exoPlayer)
//                    Box {

//                        Box(
//                            modifier = Modifier
//                                .fillMaxWidth()
//                                .height(4.dp)
//                                .background(Color(0x4DFFFFFF))
//                        )
//                        Box(
//                            modifier = Modifier
//                                .width(200.dp)
//                                .height(4.dp)
//                                .background(Color(0x33FFFFFF))
//                        )
//                        Box(
//                            modifier = Modifier
//                                .width(160.dp)
//                                .height(4.dp)
//                                .background(Color(0xFF0A62F5))
//                        )
//                    }
                    Row(
                        horizontalArrangement = Arrangement.SpaceBetween,
                        modifier = Modifier
                            .fillMaxWidth()
                    ) {
                        Row(
                            horizontalArrangement = Arrangement.Start,
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier
                                .fillMaxHeight()
                                .width(32.dp)
//                                .padding(12.dp)
                        ) {
                            Image(painter = painterResource(R.drawable.ic_cc_off),
                                contentDescription = null,
                                modifier = Modifier.size(20.dp))
                        }
                        Row(
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier
                                .fillMaxHeight()
//                                .width(32.dp)
//                                .padding(12.dp)
                        ) {

                            PreviousButton(
                                modifier = Modifier.size(20.dp),
                                exoPlayer
                            )
                            PlayPauseButton(
                                modifier = Modifier.size(32.dp),
                                exoPlayer
                            )
                            NextButton(
                                modifier = Modifier
                                    .size(20.dp),
//                                    .padding(0.dp),
                                exoPlayer
                            )
                        }
                        Row(
                            horizontalArrangement = Arrangement.End,
                            verticalAlignment = Alignment.CenterVertically,
                            modifier = Modifier
                                .fillMaxHeight()
                                .width(32.dp)
//                                .padding(12.dp)
                        ) {
                            PlaybackSpeedPopUpButton(
                                modifier = Modifier,
                                exoPlayer
                            )
                        }
                    }
                }
            }
        }
    }
}

@Preview
@Composable
fun PlayerControlsAVPreview() {
    Box(modifier = Modifier
        .fillMaxSize()
        .background(Color.Gray)) {
        PlayerControlsAV(modifier = Modifier.fillMaxSize())
    }
}
