package com.futo.fcast.receiver.views

import android.view.View
import androidx.annotation.OptIn
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.aspectRatio
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.compose.ui.viewinterop.AndroidView
import androidx.constraintlayout.compose.ConstraintLayout
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_MUSIC
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.PlayerView
import androidx.media3.ui.SubtitleView
import coil3.compose.AsyncImage
import com.futo.fcast.receiver.PlayerActivity
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.DelayedLoadingIndicator
import com.futo.fcast.receiver.composables.PlayerActivityViewConnectionMonitor
import com.futo.fcast.receiver.composables.PlayerState
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.noRippleClickable
import com.futo.fcast.receiver.composables.rememberPlayerState
import com.futo.fcast.receiver.models.PlayerActivityViewModel

@OptIn(UnstableApi::class)
@Composable
fun PlayerActivity(viewModel: PlayerActivityViewModel) {
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
    ) {
        val context = LocalContext.current
        val playerState =
            if (viewModel.exoPlayer != null) rememberPlayerState(viewModel.exoPlayer!!) else previewPlayerState

        PlayerActivityViewConnectionMonitor(context)

        ConstraintLayout(
            modifier = Modifier
                .fillMaxSize()
                .noRippleClickable {
                    if (viewModel.errorMessage == null) {
                        viewModel.showControls = !viewModel.showControls
                        PlayerActivity.instance?.uiHideControlsTimerStateChange()
                    }
                },
        ) {
            val (imageRef, playerRef, subtitlesRef, controlsRef, errorTextRef) = createRefs()

            // Notes:
            // * Cannot use PlayerSurface since it does not work for images
            // * Emulator tends to have issues rendering videos (e.g. when playing nonstandard
            //   resolutions like 854x480 or having invalid colors/artifacting when seeking)
            AndroidView(
                factory = {
                    PlayerView(context).apply {
                        this.player = viewModel.exoPlayer
                        this.useController = false
                        this.subtitleView?.visibility = View.GONE
                        this.artworkDisplayMode = PlayerView.ARTWORK_DISPLAY_MODE_OFF

//                    this.useController = true
//                    setShowSubtitleButton(true)
//                    this.setShowBuffering(SHOW_BUFFERING_ALWAYS)
//                    exoPlayer.setResizeMode(AspectRatioFrameLayout.RESIZE_MODE_FIT)
                    }
                },
                update = { view ->
                    view.player = viewModel.exoPlayer
                    view.useController = false
                    view.subtitleView?.visibility = View.GONE
                },
                modifier = Modifier
                    .fillMaxWidth()
                    .aspectRatio(16f / 9f)
                    .constrainAs(playerRef) {
                        top.linkTo(parent.top)
                        bottom.linkTo(parent.bottom)
                        start.linkTo(parent.start)
                        end.linkTo(parent.end)
                    }
            )

            AndroidView(
                factory = {
                    SubtitleView(context).apply {
                        this.setCues(playerState.cues)
                    }
                },
                update = { view ->
                    view.setCues(playerState.cues)
                },
                modifier = Modifier.constrainAs(subtitlesRef) {
                    if (viewModel.showControls) {
                        bottom.linkTo(controlsRef.top, margin = (-120).dp)
                    } else {
                        bottom.linkTo(parent.bottom, margin = 10.dp)
                    }
                }
            )

            if (viewModel.isLoading) {
                // TODO: Replace with new background load screen in next update
                Box(
                    modifier = Modifier
                        .fillMaxSize()
                        .background(Color.Black)
                )
            }
            if (viewModel.errorMessage == null && playerState.mediaType == MEDIA_TYPE_MUSIC && playerState.mediaThumbnail != null) {
                AsyncImage(
                    model = playerState.mediaThumbnail,
                    contentDescription = null,
                    modifier = Modifier
                        .constrainAs(imageRef) {
                            top.linkTo(parent.top)
                            bottom.linkTo(parent.bottom)
                            start.linkTo(parent.start)
                            end.linkTo(parent.end)
                        }
                        .fillMaxSize(0.5f)
                )
            }

            if (viewModel.isLoading || playerState.isBuffering) {
                DelayedLoadingIndicator(
                    modifier = Modifier
                        .size(80.dp)
                        .alpha(0.5f)
                        .constrainAs(imageRef) {
                            top.linkTo(parent.top)
                            bottom.linkTo(parent.bottom)
                            start.linkTo(parent.start)
                            end.linkTo(parent.end)
                        }
                )
            } else if (viewModel.isIdle || viewModel.errorMessage != null || (playerState.mediaType == MEDIA_TYPE_MUSIC && playerState.mediaThumbnail == null)) {
                Box(
                    Modifier
                        .fillMaxSize()
                        .background(Color.Black)
                )
                Image(
                    painter = painterResource(R.drawable.ic_icon),
                    contentDescription = null,
                    modifier = Modifier
                        .size(80.dp)
                        .constrainAs(imageRef) {
                            top.linkTo(parent.top)
                            bottom.linkTo(parent.bottom)
                            start.linkTo(parent.start)
                            end.linkTo(parent.end)
                        },
                )
            }

            AnimatedVisibility(
                visible = viewModel.showControls,
                enter = fadeIn(animationSpec = tween(durationMillis = 200)),
                exit = fadeOut(animationSpec = tween(durationMillis = 200)),
                modifier = Modifier
                    .constrainAs(controlsRef) {
                        bottom.linkTo(parent.bottom)
                    },
            ) {
                PlayerControlsAV(
                    viewModel,
                    Modifier,
                    playerState
                )
            }

            if (viewModel.errorMessage != null) {
                ThemedText(
                    text = viewModel.errorMessage!!,
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(start = 8.dp, end = 8.dp, bottom = 10.dp)
                        .constrainAs(errorTextRef) {
                            start.linkTo(parent.start)
                            end.linkTo(parent.end)
                            bottom.linkTo(parent.bottom)
                        },
                    fontSize = 16.sp,
                )
            }
        }
    }
}

val previewPlayerState = PlayerState(
    1000L * 30,
    1000L * 60,
    1000L * 45,
    true,
    isBuffering = false,
    true,
    isLive = false,
    isLiveSeekable = false,
    "Video Title",
    null,
    0,
    null,
)

@Preview
@Composable
@OptIn(UnstableApi::class)
fun PlayerActivityPreview() {
    val viewModel = PlayerActivityViewModel()
    viewModel.errorMessage = "This is a test message"
    viewModel.showControls = false
    viewModel.isLoading = false
    viewModel.isIdle = true

    PlayerActivity(viewModel)
}
