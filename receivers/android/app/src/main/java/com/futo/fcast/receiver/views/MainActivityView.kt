package com.futo.fcast.receiver.views

import android.content.res.Configuration
import androidx.annotation.OptIn
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxHeight
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Button
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.alpha
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.asImageBitmap
import androidx.compose.ui.graphics.painter.BitmapPainter
import androidx.compose.ui.graphics.painter.ColorPainter
import androidx.compose.ui.graphics.painter.Painter
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.tooling.preview.Devices.TV_720p
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import androidx.constraintlayout.compose.ConstraintLayout
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.ui.compose.PlayerSurface
import androidx.media3.ui.compose.SURFACE_TYPE_SURFACE_VIEW
import androidx.media3.ui.compose.modifiers.resizeWithContentScale
import androidx.media3.ui.compose.state.rememberPresentationState
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.MainActivityViewConnectionMonitor
import com.futo.fcast.receiver.composables.Spinner
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.composables.outfitFontFamilyExtraBold
import com.futo.fcast.receiver.models.MainActivityViewModel

@Composable
fun TitleStatusGroupView(viewModel: MainActivityViewModel, modifier: Modifier) {
//                Spacer(Modifier.fillMaxWidth().height(spacerSize))
    Row(
        modifier = modifier,
        verticalAlignment = Alignment.CenterVertically
    ) {
        Image(
            painter = painterResource(R.drawable.ic_icon),
            contentDescription = null,
            modifier = Modifier
                .size(55.dp)
        )
        Text(
            text = "FCast",
            modifier = Modifier
                .padding(start = 15.dp),
            color = Color.White,
            fontSize = 60.sp,
            fontFamily = outfitFontFamilyExtraBold,
            textAlign = TextAlign.Center,
            style = TextStyle(
                brush = Brush.verticalGradient(listOf(Color.White, Color(0xFFD3D3D3)))
            )
        )
    }

//                Spacer(Modifier.fillMaxWidth().height(spacerSize))
    Row(
//                    modifier = Modifier.weight(0.5f),
        verticalAlignment = Alignment.CenterVertically
    ) {
        val connectionStatus = when (viewModel.connections.size) {
            0 -> stringResource(R.string.waiting_for_connection)
            1 -> stringResource(R.string.connected)
            else -> stringResource(R.string.connected_multiple)
        }

        Text(
            text = connectionStatus,
            modifier = Modifier
                .padding(end = 8.dp),
            color = Color.White,
            fontSize = 16.sp,
            fontFamily = interFontFamily,
            fontWeight = FontWeight.Normal,
            textAlign = TextAlign.Center,
        )

        if (viewModel.connections.isEmpty()) {
            Spinner(
                modifier = Modifier
                    .size(35.dp),
                R.drawable.ic_loader_animated
            )
        }
        else {
            // todo: image / animation update
            Image(
                painter = painterResource(R.drawable.ic_update_success),
                contentDescription = null,
                modifier = Modifier
                    .size(35.dp)
            )
        }
    }
}

@Composable
fun ConnectionInfoView(viewModel: MainActivityViewModel, modifier: Modifier) {
    val dividerPadding = 10.dp

//                Spacer(Modifier.fillMaxWidth().height(spacerSize))
    Surface(
        modifier = modifier,
        color = Color(0x801D1D1D),
        shape = RoundedCornerShape(10.dp),
        border = BorderStroke(1.dp, Color(0xFF2E2E2E))
    ) {
        Column(modifier = Modifier
            .padding(horizontal = 15.dp, vertical = 10.dp),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            Text(
                text = stringResource(R.string.connection_information),
                modifier = Modifier
                    .padding(vertical = 0.dp),
                color = Color.White,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Bold,
                textAlign = TextAlign.Center,
            )
            Spacer(
                modifier = Modifier
                    .padding(vertical = dividerPadding)
                    .height(1.dp)
                    .fillMaxWidth()
                    .background(Color.Gray)
            )

            if (viewModel.showQR) {
                Text(
                    text = stringResource(R.string.scan_to_connect),
                    modifier = Modifier
                        .padding(bottom = 10.dp),
                    color = Color.White,
                    fontSize = 14.sp,
                    fontFamily = interFontFamily,
                    fontWeight = FontWeight.Bold,
                    textAlign = TextAlign.Center,
                )
                Image(
                    painter = if (viewModel.imageQR != null)
                        BitmapPainter(viewModel.imageQR!!)
                    else ColorPainter(Color.Gray),
                    contentDescription = null,
                    modifier = Modifier
                        .size(200.dp)
                )
                Text(
                    text = stringResource(R.string.sender_app_download),
                    modifier = Modifier
                        .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
                    color = Color.White,
                    fontSize = 14.sp,
                    fontFamily = interFontFamily,
                    fontWeight = FontWeight.Normal,
                    textAlign = TextAlign.Center,
                )

                Text(
                    text = stringResource(R.string.connection_details),
                    modifier = Modifier
                        .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
                    color = Color.White,
                    fontSize = 14.sp,
                    fontFamily = interFontFamily,
                    fontWeight = FontWeight.Normal,
                    textAlign = TextAlign.Center,
                )
                Spacer(
                    modifier = Modifier
                        .padding(vertical = 14.dp)
                        .height(1.dp)
                        .fillMaxWidth()
                        .background(Color.Gray)
                )
            }

            Text(
                text = stringResource(R.string.ips),
                modifier = Modifier
                    .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
                color = Color.White,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Normal,
                textAlign = TextAlign.Center,
            )
            Text(
                text = viewModel.textIPs,
                modifier = Modifier
                    .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
                color = Color.White,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Normal,
                textAlign = TextAlign.Center,
            )

            Spacer(modifier = Modifier.padding(vertical = 14.dp))
            Text(
                text = stringResource(R.string.port),
                modifier = Modifier
                    .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
                color = Color.White,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Normal,
                textAlign = TextAlign.Center,
            )
            Text(
                text = viewModel.textPorts,
                modifier = Modifier
                    .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
                color = Color.White,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Normal,
                textAlign = TextAlign.Center,
            )

        }
    }
}

@OptIn(UnstableApi::class)
@Composable
fun MainActivity(viewModel: MainActivityViewModel, exoPlayer: Player? = null) {
    val context = LocalContext.current
    val configuration = LocalConfiguration.current
    val isPortrait = configuration.orientation == Configuration.ORIENTATION_PORTRAIT
    val spacerSize = 20.dp

    val presentationState = rememberPresentationState(exoPlayer)
    MainActivityViewConnectionMonitor(viewModel, context)

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black)
    ) {
        PlayerSurface(
            player = exoPlayer,
            surfaceType = SURFACE_TYPE_SURFACE_VIEW,
            modifier = Modifier
                .fillMaxSize()
                .resizeWithContentScale(ContentScale.Crop, presentationState.videoSizeDp)
        )

        if (isPortrait) {
            Column(
                Modifier.padding(horizontal = 20.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                TitleStatusGroupView(viewModel, Modifier.weight(0.5f))
                ConnectionInfoView(viewModel, Modifier.weight(4f))
                Spacer(Modifier.fillMaxWidth().height(spacerSize))
                UpdateView(viewModel, modifier = Modifier.weight(1f))
            }
        }
        else {
            Row {
                Column(
                    modifier = Modifier
                        .padding(horizontal = 140.dp, vertical = 80.dp)
                        .weight(1f),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    TitleStatusGroupView(viewModel, Modifier.weight(0.5f))
                    Spacer(Modifier.height(spacerSize))
                    Surface(
                        modifier = Modifier.weight(1f),
                        color = Color(0x801D1D1D),
                        shape = RoundedCornerShape(10.dp),
                        border = BorderStroke(1.dp, Color(0xFF2E2E2E))
                    ) {
                        UpdateView(viewModel, modifier = Modifier.weight(1f))
                    }
                }
                Column(
                    modifier = Modifier
                        .padding(horizontal = 140.dp, vertical = 80.dp)
                        .weight(1f),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    ConnectionInfoView(viewModel, Modifier.weight(4f))
                }
            }
        }
    }
}


@Preview(device = TV_720p)
@Composable
fun MainActivityLandscapePreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateStatus = stringResource(R.string.update_status)
    viewModel.updateAvailable = true
    viewModel.textIPs = "123.456.7.890"
    viewModel.textPorts = "46899 (TCP), 46898 (WS)"
    viewModel.showQR = true
//    viewModel.updating = true
    MainActivity(viewModel)
}

@Preview
@Composable
fun MainActivityPortraitPreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateStatus = stringResource(R.string.update_status)
    viewModel.updateAvailable = true
    viewModel.textIPs = "123.456.7.890"
    viewModel.textPorts = "46899 (TCP), 46898 (WS)"
    viewModel.showQR = true
//    viewModel.updating = true
    MainActivity(viewModel)
}
