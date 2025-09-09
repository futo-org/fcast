package com.futo.fcast.receiver.views

import android.annotation.SuppressLint
import android.content.res.Configuration
import androidx.annotation.OptIn
import androidx.annotation.StringRes
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
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
import androidx.compose.foundation.text.ClickableText
import androidx.compose.material3.Button
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
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
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
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
import com.futo.fcast.receiver.NetworkInterfaceData
import com.futo.fcast.receiver.NetworkInterfaceType
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.composables.MainActivityViewConnectionMonitor
import com.futo.fcast.receiver.composables.Spinner
import com.futo.fcast.receiver.composables.frontendConnections
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.composables.outfitFontFamilyExtraBold
import com.futo.fcast.receiver.models.MainActivityViewModel

@Composable
fun ThemedText(text: String, modifier: Modifier = Modifier, fontWeight: FontWeight = FontWeight.Normal) {
    Text(
        text = text,
        modifier = modifier,
        color = Color.White,
        fontSize = 14.sp,
        fontFamily = interFontFamily,
        fontWeight = fontWeight,
        textAlign = TextAlign.Center,
    )
}

@Composable
fun SenderAppDownloadText() {
    val uriHandler = LocalUriHandler.current
    val text = stringResource(R.string.sender_app_download_text)
    val url = stringResource(R.string.sender_app_download_url)

    val annotatedString = buildAnnotatedString {
        append(text)
        addStyle(
            style = SpanStyle(
                color = Color.White,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Normal,
            ),
            start = 0,
            end = text.length + 1
        )

        append(" $url")
        addStringAnnotation(
            tag = "url",
            annotation = url,
            start = text.length + 1,
            end = text.length + url.length + 2
        )
        addStyle(
            style = SpanStyle(
                color = Color.Blue,
                fontSize = 14.sp,
                fontFamily = interFontFamily,
                fontWeight = FontWeight.Normal,
                textDecoration = TextDecoration.Underline
            ),
            start = text.length + 1,
            end = text.length + url.length + 1
        )
    }

    ClickableText(
        text = annotatedString,
        modifier = Modifier.padding(vertical = 10.dp),
        style = TextStyle(textAlign = TextAlign.Center),
        onClick = { offset ->
            annotatedString.getStringAnnotations(tag = "url", start = offset, end = offset).firstOrNull()?.let {
                uriHandler.openUri(it.item)
            }
        }
    )
}

@Composable
fun ConnectionDetailsView(viewModel: MainActivityViewModel, modifier: Modifier) {
    if (viewModel.ipInfo.isEmpty()) {
        Text(
            text = "add no connection info error",
            modifier = Modifier
                .padding(start = 8.dp, end = 8.dp, bottom = 10.dp),
            color = Color.White,
            fontSize = 14.sp,
            fontFamily = interFontFamily,
            fontWeight = FontWeight.Normal,
            textAlign = TextAlign.Center,
        )
    }
    else {
        for (ip in viewModel.ipInfo) {
            val icon = when (ip.type) {
                NetworkInterfaceType.Wired, NetworkInterfaceType.Unknown -> R.drawable.network_light
                NetworkInterfaceType.Wireless -> {
                    // todo: review/fix ranges
                    if (ip.signalLevel != null) {
                        when {
                            ip.signalLevel == 0 || ip.signalLevel >= 90 -> R.drawable.wifi_strength_4
                            ip.signalLevel >= 70 -> R.drawable.wifi_strength_3
                            ip.signalLevel >= 50 -> R.drawable.wifi_strength_2
                            ip.signalLevel >= 30 -> R.drawable.wifi_strength_1
                            else -> R.drawable.wifi_strength_outline
                        }
                    }
                    else {
                        R.drawable.wifi_strength_3
                    }
                }
            }

            Row (
                Modifier.padding(vertical = 2.dp)
            ) {
                Image(
                    painter = painterResource(icon),
                    contentDescription = null,
                    modifier = Modifier
                        .size(18.dp)
                )
                ThemedText(ip.address, Modifier.padding(horizontal = 8.dp))
                ThemedText(ip.name)
            }

//            Row (
//                Modifier.padding(vertical = 2.dp)
//            ) {
//                Image(
//                    painter = painterResource(icon),
//                    contentDescription = null,
//                    modifier = Modifier
//                        .size(18.dp)
//                )
//                ThemedText(ip.address, Modifier.padding(horizontal = 8.dp))
//                ThemedText(ip.name)
//            }
        }

        Spacer(modifier = Modifier.padding(vertical = 10.dp))
        ThemedText(stringResource(R.string.port))
        ThemedText(viewModel.textPorts)
    }
}

@Composable
fun ConnectionStatusView(viewModel: MainActivityViewModel) {
    val isPortrait = LocalConfiguration.current.orientation == Configuration.ORIENTATION_PORTRAIT

    val connectionStatus = when (frontendConnections.size) {
        0 -> stringResource(R.string.waiting_for_connection)
        1 -> stringResource(R.string.connected)
        else -> stringResource(R.string.connected_multiple)
    }

    val textModifier = if (isPortrait) Modifier.padding(end = 8.dp) else Modifier
    ThemedText(connectionStatus, textModifier)
    Spacer(Modifier.height(20.dp))

    val iconModifier = if (isPortrait) Modifier.size(35.dp) else Modifier.size(55.dp)
    if (frontendConnections.isEmpty()) {
        Spinner(
            modifier = iconModifier,
            R.drawable.ic_loader_animated
        )
    }
    else {
        // todo: image / animation update
        Image(
            painter = painterResource(R.drawable.ic_update_success),
            contentDescription = null,
            modifier = iconModifier
        )
    }
}

@Composable
fun TitleStatusGroupView(viewModel: MainActivityViewModel, modifier: Modifier) {
    val isPortrait = LocalConfiguration.current.orientation == Configuration.ORIENTATION_PORTRAIT

    Column(
        modifier = modifier,
        verticalArrangement = Arrangement.Top,
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        Row(
            modifier = Modifier.padding(vertical = 20.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Image(
                painter = painterResource(R.drawable.ic_icon),
                contentDescription = null,
                modifier = Modifier.size(55.dp)
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

        Spacer(Modifier.fillMaxWidth().height(20.dp))
        if (isPortrait) {
            Row(
                verticalAlignment = Alignment.CenterVertically,
            ) {
                ConnectionStatusView(viewModel)
            }
        }
        else {
            ConnectionStatusView(viewModel)
        }
    }
}

@SuppressLint("ConfigurationScreenWidthHeight")
@Composable
fun ConnectionInfoView(viewModel: MainActivityViewModel, modifier: Modifier) {
    val paddingSize = 10.dp

//                Spacer(Modifier.fillMaxWidth().height(spacerSize))
    Surface(
        modifier = modifier,
        color = Color(0x801D1D1D),
        shape = RoundedCornerShape(10.dp),
        border = BorderStroke(1.dp, Color(0xFF2E2E2E))
    ) {
        Column(modifier = Modifier
            .padding(horizontal = 15.dp, vertical = paddingSize),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            ThemedText(stringResource(R.string.connection_information), Modifier.padding(top = paddingSize), FontWeight.Bold)
            Spacer(
                modifier = Modifier
                    .padding(vertical = paddingSize)
                    .height(1.dp)
                    .fillMaxWidth()
                    .background(Color.Gray)
            )

            if (viewModel.showQR) {
                val isPortrait = LocalConfiguration.current.orientation == Configuration.ORIENTATION_PORTRAIT
                val configuration = LocalConfiguration.current
                val density = LocalDensity.current

                val width = with(density) { configuration.screenWidthDp.dp.toPx() }.toInt()
                val height = with(density) { configuration.screenHeightDp.dp.toPx() }.toInt()

                var qrSize = 170.dp

                // todo centralize handling of qr creation and display sizes
                if (isPortrait) {
                    if (height >= 2560 || width >= 1440) {
                        qrSize = 165.dp
                    }
                    if ((height >= 1920 && height < 2560) || (width >= 1080 && width < 1440)) {
                        qrSize = 125.dp
                    }
                    if ((height >= 1280 && height < 1920) || (width >= 720 && width < 1080)) {
                        qrSize = 85.dp
                    }
                    if (height < 1280 || width < 720) {
                        qrSize = 60.dp
                    }
                }
                else {
                    if (width >= 2560 || height >= 1440) {
                        qrSize = 165.dp
                    }
                    if ((width >= 1920 && width < 2560) || (height >= 1080 && height < 1440)) {
                        qrSize = 125.dp
                    }
                    if ((width >= 1280 && width < 1920) || (height >= 720 && height < 1080)) {
                        qrSize = 85.dp
                    }
                    if (width < 1280 || height < 720) {
                        qrSize = 60.dp
                    }
                }


//                val qrSize = when {
//                    (width >= 2560 || height >= 1440) -> 200.dp
//                    (width >= 1920) || (height >= 1080) -> 150.dp
//                    (width >= 1280) || (height >= 720) -> 90.dp
//                    else -> 60.dp
//                }

//                ThemedText(width.toString() + " " + height.toString())

                ThemedText(stringResource(R.string.scan_to_connect), Modifier.padding(bottom = paddingSize), FontWeight.Bold)
                Image(
                    painter = if (viewModel.imageQR != null)
                        BitmapPainter(viewModel.imageQR!!)
                    else ColorPainter(Color.Gray),
                    contentDescription = null,
                    modifier = Modifier
                        .size(qrSize)
                )
                SenderAppDownloadText()

                ThemedText(stringResource(R.string.connection_details), Modifier.padding(top = paddingSize), FontWeight.Bold)
                Spacer(
                    modifier = Modifier
                        .padding(top = paddingSize, bottom = paddingSize - 2.dp)
                        .height(1.dp)
                        .fillMaxWidth()
                        .background(Color.Gray)
                )
            }

            ConnectionDetailsView(viewModel, modifier)
        }
    }
}

@OptIn(UnstableApi::class)
@Composable
fun MainActivity(viewModel: MainActivityViewModel, exoPlayer: Player? = null) {
    val context = LocalContext.current
    val isPortrait = LocalConfiguration.current.orientation == Configuration.ORIENTATION_PORTRAIT
    val spacerSize = 20.dp

    val presentationState = rememberPresentationState(exoPlayer)
    MainActivityViewConnectionMonitor(context)

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
                Modifier.padding(horizontal = 40.dp),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                TitleStatusGroupView(viewModel, Modifier.weight(2f))
                ConnectionInfoView(viewModel, Modifier.weight(4f))
                Spacer(Modifier.fillMaxWidth().height(spacerSize))
                UpdateView(viewModel, modifier = Modifier.weight(1f))
            }
        }
        else {
            Row(
                modifier = Modifier.padding(vertical = 40.dp)
            ) {
                val columnPadding = 80.dp

                Column(
                    modifier = Modifier
//                        .padding(horizontal = 140.dp, vertical = 80.dp)
//                        .padding(horizontal = 40.dp, vertical = 40.dp)
                        .padding(horizontal = columnPadding)
                        .weight(1f),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    TitleStatusGroupView(viewModel, Modifier.weight(2f).fillMaxWidth())
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
//                        .padding(horizontal = 140.dp, vertical = 80.dp)
//                        .padding(horizontal = 40.dp, vertical = 40.dp)
//                        .padding(start = 40.dp)
                        .padding(horizontal = columnPadding)
                        .weight(1f),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    ConnectionInfoView(viewModel, Modifier.weight(4f))
                }
            }
        }
    }
}

@SuppressLint("UnrememberedMutableState")
@Preview(device = TV_720p)
@Composable
fun MainActivityLandscapePreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateStatus = stringResource(R.string.update_status)
    viewModel.updateAvailable = true
    viewModel.textPorts = "46899 (TCP), 46898 (WS)"
    viewModel.showQR = true
//    viewModel.updating = true

    viewModel.ipInfo = mutableStateListOf(NetworkInterfaceData(
        NetworkInterfaceType.Wired, "Ethernet", "123.456.7.890", null
    ))
//    viewModel.ipInfo = mutableStateListOf<NetworkInterfaceData>()

    MainActivity(viewModel)
}

@SuppressLint("UnrememberedMutableState")
@Preview
@Composable
fun MainActivityPortraitPreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateStatus = stringResource(R.string.update_status)
    viewModel.updateAvailable = true
    viewModel.textPorts = "46899 (TCP), 46898 (WS)"
    viewModel.showQR = true
//    viewModel.updating = true
    viewModel.ipInfo = mutableStateListOf(NetworkInterfaceData(
        NetworkInterfaceType.Wired, "Ethernet", "123.456.7.890", null
    ))
//    viewModel.ipInfo = mutableStateListOf<NetworkInterfaceData>()


    MainActivity(viewModel)
}
