package com.futo.fcast.receiver.views

import androidx.annotation.OptIn
import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.core.tween
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.constraintlayout.compose.ConstraintLayout
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_MIXED
import androidx.media3.common.MediaMetadata.MEDIA_TYPE_MUSIC
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.compose.PlayerSurface
import androidx.media3.ui.compose.SURFACE_TYPE_SURFACE_VIEW
import androidx.media3.ui.compose.modifiers.resizeWithContentScale
import coil3.compose.AsyncImage
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.PlayerActivityViewConnectionMonitor
import com.futo.fcast.receiver.composables.PlayerState
import com.futo.fcast.receiver.composables.Spinner
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.composables.noRippleClickable
import com.futo.fcast.receiver.composables.rememberPlayerState
import com.futo.fcast.receiver.models.PlayerActivityViewModel

@OptIn(UnstableApi::class)
@Composable
fun CustomPlayerViewScreen(viewModel: PlayerActivityViewModel, exoPlayer: Player? = null) {
    val context = LocalContext.current
//    val presentationState = rememberPresentationState(exoPlayer)
//    val scaledModifier = Modifier.resizeWithContentScale(ContentScale.Fit, presentationState.videoSizeDp)
    val playerState = if (exoPlayer != null) rememberPlayerState(exoPlayer) else previewPlayerState
    val scaledModifier =
        Modifier.resizeWithContentScale(ContentScale.Fit, playerState.currentVideoSize)

    PlayerActivityViewConnectionMonitor(context)

    Box(Modifier.fillMaxSize()) {
        // todo: fix issue when seeking causes video size change, but content is not scaled to surface size...
        // occurs also in old receiver build, but perhaps resize mode is not configured correctly as it is in grayjay
        PlayerSurface(
            player = exoPlayer,
            surfaceType = SURFACE_TYPE_SURFACE_VIEW,
            modifier = scaledModifier.noRippleClickable {
                viewModel.showControls = !viewModel.showControls
            },
        )

//        AndroidView(
//            factory = { context ->
//                PlayerView(context).apply {
//                    player = exoPlayer
//                    resizeMode = AspectRatioFrameLayout.RESIZE_MODE_FIT
//                }
//            },
//            update = { view ->
////                view.player = exoPlayer
//            },
//            modifier = Modifier.fillMaxSize()
//        )

        if (viewModel.isLoading) {
            // TODO: Replace with new background load screen in next update
            Box(
                Modifier
                    .matchParentSize()
                    .background(Color.Black)
            )
            ConstraintLayout(
                modifier = Modifier
                    .fillMaxSize()
                    .background(Color(0x66000000))
            ) {
                val imageRef = createRef()

                Spinner(
                    Modifier
                        .size(80.dp)
//                .padding(start = 8.dp)
                        .alpha(0.5f)
                        .constrainAs(imageRef) {
                            top.linkTo(parent.top)
                            bottom.linkTo(parent.bottom)
                            start.linkTo(parent.start)
                            end.linkTo(parent.end)
                        })
            }
        } else if (!viewModel.isLoading && playerState.mediaType == MEDIA_TYPE_MUSIC && playerState.mediaThumbnail != null) {
            AsyncImage(
                model = playerState.mediaThumbnail,
                contentDescription = null,
                modifier = Modifier.fillMaxSize()
            )
        } else if (!viewModel.isLoading && playerState.mediaType == MEDIA_TYPE_MIXED) {
            AsyncImage(
                model = viewModel.playMessage?.url,
                contentDescription = null,
                modifier = Modifier.fillMaxSize()
            )
        } else if (viewModel.isIdle) {
            Box(
                Modifier
                    .matchParentSize()
                    .background(Color.Black)
            )
            ConstraintLayout(
                modifier = Modifier
                    .fillMaxSize()
                    .background(Color(0x66000000))
            ) {
                val imageRef = createRef()

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
        }

        AnimatedVisibility(
            visible = viewModel.showControls,
            enter = fadeIn(animationSpec = tween(durationMillis = 200)),
            exit = fadeOut(animationSpec = tween(durationMillis = 200))
        ) {
            PlayerControlsAV(
                viewModel,
                Modifier.fillMaxSize(),
                exoPlayer,
                playerState
            )
        }
    }
}

@Composable
fun ConstraintLayoutGroup(viewModel: PlayerActivityViewModel) {
    val visible = viewModel.statusMessage != null

    if (visible) {
        ConstraintLayout(
            modifier = Modifier
                .fillMaxSize()
                .background(Color(0x66000000))
        ) {
            val textRef = createRef()

            if (viewModel.statusMessage != null) {
                Text(
                    text = viewModel.statusMessage!!,
                    modifier = Modifier
                        .fillMaxWidth()
                        .padding(start = 8.dp, end = 8.dp, bottom = 10.dp)
                        .constrainAs(textRef) {
                            start.linkTo(parent.start)
                            end.linkTo(parent.end)
                            bottom.linkTo(parent.bottom)
                        },
                    color = Color.White,
                    fontSize = 16.sp,
                    fontFamily = interFontFamily,
                    fontWeight = FontWeight.Normal,
                    textAlign = TextAlign.Center,

                    )
            }
        }
    }
}

@Composable
fun PlayerActivity(viewModel: PlayerActivityViewModel, exoPlayer: Player? = null) {
    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
    ) {
        CustomPlayerViewScreen(viewModel, exoPlayer)
        ConstraintLayoutGroup(viewModel)
    }
}

val previewPlayerState = PlayerState(
    null,
    1000L * 30,
    1000L * 60,
    1000L * 45,
    true,
    true,
    false,
    "Video Title",
    null,
    0
)

@Preview
@Composable
@OptIn(UnstableApi::class)
fun PlayerActivityPreview() {
    val viewModel = PlayerActivityViewModel()
    viewModel.statusMessage = "This is a test message"
    viewModel.showControls = false
    viewModel.isLoading = false
    viewModel.isIdle = true

    PlayerActivity(viewModel)
}

