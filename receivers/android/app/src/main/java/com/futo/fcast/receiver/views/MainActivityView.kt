package com.futo.fcast.receiver.views

import android.annotation.SuppressLint
import android.content.res.Configuration
import androidx.annotation.OptIn
import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.width
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.ClickableText
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.painter.BitmapPainter
import androidx.compose.ui.graphics.painter.ColorPainter
import androidx.compose.ui.layout.ContentScale
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.platform.LocalUriHandler
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.res.stringResource
import androidx.compose.ui.text.SpanStyle
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.buildAnnotatedString
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextDecoration
import androidx.compose.ui.tooling.preview.Devices.TV_720p
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
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
import com.futo.fcast.receiver.composables.ScreenSize
import com.futo.fcast.receiver.composables.ThemedText
import com.futo.fcast.receiver.composables.WipeEffect
import com.futo.fcast.receiver.composables.colorCardBackground
import com.futo.fcast.receiver.composables.colorPrimary
import com.futo.fcast.receiver.composables.frontendConnections
import com.futo.fcast.receiver.composables.getDefaultFontSize
import com.futo.fcast.receiver.composables.getDefaultSpacerHeight
import com.futo.fcast.receiver.composables.getQrSize
import com.futo.fcast.receiver.composables.getScreenResolution
import com.futo.fcast.receiver.composables.getScreenSize
import com.futo.fcast.receiver.composables.interFontFamily
import com.futo.fcast.receiver.composables.outfitFontFamilyExtraBold
import com.futo.fcast.receiver.composables.strokeCardBorder
import com.futo.fcast.receiver.models.MainActivityViewModel
import com.futo.fcast.receiver.models.UpdateState

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
                fontSize = getDefaultFontSize(),
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
                color = colorPrimary,
                fontSize = getDefaultFontSize(),
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
            annotatedString.getStringAnnotations(tag = "url", start = offset, end = offset)
                .firstOrNull()?.let {
                    uriHandler.openUri(it.item)
                }
        }
    )
}

@Composable
fun ConnectionDetailsView(viewModel: MainActivityViewModel, modifier: Modifier) {
    val (iconSize, paddingSize) = when (getScreenSize((getScreenResolution()))) {
        ScreenSize.Tiny -> Pair(14.dp, 6.dp)
        ScreenSize.Small -> Pair(14.dp, 6.dp)
        ScreenSize.Medium -> Pair(18.dp, 8.dp)
        ScreenSize.Large -> Pair(18.dp, 8.dp)
    }

    for (ip in viewModel.ipInfo) {
        val icon = when (ip.type) {
            NetworkInterfaceType.Wired, NetworkInterfaceType.Unknown -> R.drawable.ic_network_light
            NetworkInterfaceType.Wireless -> {
                if (ip.signalLevel != null) {
                    when (ip.signalLevel) {
                        0 -> R.drawable.ic_wifi_strength_outline
                        1 -> R.drawable.ic_wifi_strength_1
                        2 -> R.drawable.ic_wifi_strength_2
                        3 -> R.drawable.ic_wifi_strength_3
                        else -> R.drawable.ic_wifi_strength_4
                    }
                } else {
                    R.drawable.ic_wifi_strength_3
                }
            }
        }

        Row(
            Modifier.padding(vertical = 2.dp)
        ) {
            Image(
                painter = painterResource(icon),
                contentDescription = null,
                modifier = Modifier
                    .size(iconSize)
            )
            ThemedText(ip.address, Modifier.padding(horizontal = paddingSize))
            ThemedText(ip.name)
        }

//        Row(
//            Modifier.padding(vertical = 2.dp)
//        ) {
//            Image(
//                painter = painterResource(icon),
//                contentDescription = null,
//                modifier = Modifier
//                    .size(iconSize)
//            )
//            ThemedText(ip.address, Modifier.padding(horizontal = paddingSize))
//            ThemedText(ip.name)
//        }
    }

    Spacer(modifier = Modifier.padding(vertical = paddingSize))
    ThemedText(stringResource(R.string.port))
    ThemedText(viewModel.textPorts)
}

@Composable
fun ConnectionStatusView(viewModel: MainActivityViewModel, modifier: Modifier) {
    val isPortrait = LocalConfiguration.current.orientation == Configuration.ORIENTATION_PORTRAIT

    val connectionStatus = when (frontendConnections.size) {
        0 -> stringResource(R.string.waiting_for_connection)
        1 -> stringResource(R.string.connected)
        else -> stringResource(R.string.connected_multiple)
    }

    val textModifier = if (isPortrait) Modifier.padding(end = 15.dp) else modifier
    ThemedText(connectionStatus, textModifier)
    Spacer(Modifier.height(getDefaultSpacerHeight()))

    val iconSize = if (isPortrait) {
        when (getScreenSize((getScreenResolution()))) {
            ScreenSize.Tiny -> 25.dp
            ScreenSize.Small -> 25.dp
            ScreenSize.Medium -> 35.dp
            ScreenSize.Large -> 35.dp
        }
    } else {
        when (getScreenSize((getScreenResolution()))) {
            ScreenSize.Tiny -> 45.dp
            ScreenSize.Small -> 45.dp
            ScreenSize.Medium -> 55.dp
            ScreenSize.Large -> 55.dp
        }
    }
    val wipePercentage = animateFloatAsState(
        targetValue = if (frontendConnections.isNotEmpty()) 1f else 0.4f,
        animationSpec = tween(durationMillis = 500)
    )

    if (frontendConnections.isEmpty()) {
        val strokeWidth = when (getScreenSize((getScreenResolution()))) {
            ScreenSize.Tiny -> 4.dp
            ScreenSize.Small -> 4.dp
            ScreenSize.Medium -> 5.dp
            ScreenSize.Large -> 5.dp
        }

        CircularProgressIndicator(
            modifier = Modifier.size(iconSize),
            color = Color.White,
            strokeWidth = strokeWidth
        )
    } else {
        Box(
            modifier = Modifier
                .size(iconSize)
                .clip(CircleShape)
                .background(colorPrimary)
        ) {
            Image(
                painter = painterResource(R.drawable.ic_checked),
                contentDescription = null,
                modifier = Modifier
                    .size(iconSize)
                    .clip(WipeEffect(wipePercentage))
            )
        }
    }
}

@Composable
fun TitleView(viewModel: MainActivityViewModel, modifier: Modifier) {
    Column(
        modifier = modifier,
        verticalArrangement = Arrangement.Top,
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        val (rowPadding, titleSize) = when (getScreenSize((getScreenResolution()))) {
            ScreenSize.Tiny -> Pair(5, 40)
            ScreenSize.Small -> Pair(5, 40)
            ScreenSize.Medium -> Pair(10, 55)
            ScreenSize.Large -> Pair(10, 55)
        }

        Row(
            modifier = Modifier.padding(vertical = rowPadding.dp),
            verticalAlignment = Alignment.CenterVertically
        ) {
            Image(
                painter = painterResource(R.drawable.ic_icon),
                contentDescription = null,
                modifier = Modifier.size(titleSize.dp)
            )
            Text(
                text = "FCast",
                modifier = Modifier
                    .padding(start = 15.dp),
                color = Color.White,
                fontSize = (titleSize + 5).sp,
                fontFamily = outfitFontFamilyExtraBold,
                textAlign = TextAlign.Center,
                style = TextStyle(
                    brush = Brush.verticalGradient(listOf(Color.White, Color(0xFFD3D3D3)))
                )
            )
        }
    }
}

@SuppressLint("ConfigurationScreenWidthHeight")
@Composable
fun ConnectionInfoView(viewModel: MainActivityViewModel, modifier: Modifier) {
    val paddingSize = when (getScreenSize((getScreenResolution()))) {
        ScreenSize.Tiny -> 5.dp
        ScreenSize.Small -> 5.dp
        ScreenSize.Medium -> 10.dp
        ScreenSize.Large -> 10.dp
    }

    Surface(
        modifier = modifier,
        color = colorCardBackground,
        shape = RoundedCornerShape(10.dp),
        border = strokeCardBorder
    ) {
        Column(
            modifier = Modifier
                .padding(horizontal = 15.dp, vertical = paddingSize),
            horizontalAlignment = Alignment.CenterHorizontally
        ) {
            ThemedText(
                stringResource(R.string.connection_information),
                Modifier.padding(top = paddingSize),
                fontWeight = FontWeight.Bold
            )
            Spacer(
                modifier = Modifier
                    .padding(vertical = paddingSize)
                    .height(1.dp)
                    .fillMaxWidth()
                    .background(Color.Gray)
            )

            if (viewModel.ipInfo.isNotEmpty()) {
                if (viewModel.showQR) {
                    ThemedText(
                        stringResource(R.string.scan_to_connect),
                        Modifier.padding(bottom = paddingSize),
                        fontWeight = FontWeight.Bold
                    )

                    Image(
                        painter = if (viewModel.imageQR != null)
                            BitmapPainter(viewModel.imageQR!!)
                        else ColorPainter(Color.Gray),
                        contentDescription = null,
                        modifier = Modifier
                            .size(getQrSize(getScreenResolution()).dp)
                    )
                    SenderAppDownloadText()

                    ThemedText(
                        stringResource(R.string.connection_details),
                        Modifier.padding(top = paddingSize),
                        fontWeight = FontWeight.Bold
                    )
                    Spacer(
                        modifier = Modifier
                            .padding(top = paddingSize, bottom = paddingSize - 2.dp)
                            .height(1.dp)
                            .fillMaxWidth()
                            .background(Color.Gray)
                    )
                }

                ConnectionDetailsView(viewModel, modifier)
            } else {
                val iconSize = when (getScreenSize((getScreenResolution()))) {
                    ScreenSize.Tiny -> 45.dp
                    ScreenSize.Small -> 45.dp
                    ScreenSize.Medium -> 55.dp
                    ScreenSize.Large -> 55.dp
                }

                Image(
                    painter = painterResource(R.drawable.ic_error),
                    contentDescription = null,
                    modifier = Modifier.size(iconSize)
                )
                ThemedText(
                    stringResource(R.string.network_no_interfaces),
                    Modifier.padding(top = paddingSize),
                    fontWeight = FontWeight.Bold
                )
            }
        }
    }
}

@OptIn(UnstableApi::class)
@Composable
fun MainActivity(viewModel: MainActivityViewModel, exoPlayer: Player? = null) {
    val context = LocalContext.current
    val isPortrait = LocalConfiguration.current.orientation == Configuration.ORIENTATION_PORTRAIT
    val spacerSize = getDefaultSpacerHeight()
    val (connInfoWidth, updateViewWidth) = when (getScreenSize((getScreenResolution()))) {
        ScreenSize.Tiny -> Pair(255.dp, 205.dp)
        ScreenSize.Small -> Pair(255.dp, 205.dp)
        ScreenSize.Medium -> Pair(340.dp, 290.dp)
        ScreenSize.Large -> Pair(340.dp, 290.dp)
    }
    val rootPadding = when (getScreenSize((getScreenResolution()))) {
        ScreenSize.Tiny -> 30.dp
        ScreenSize.Small -> 30.dp
        ScreenSize.Medium -> 40.dp
        ScreenSize.Large -> 40.dp
    }
    val (updateViewPaddingWidth, updateViewPaddingHeight) = when (getScreenSize((getScreenResolution()))) {
        ScreenSize.Tiny -> Pair(10.dp, 5.dp)
        ScreenSize.Small -> Pair(10.dp, 5.dp)
        ScreenSize.Medium -> Pair(15.dp, 10.dp)
        ScreenSize.Large -> Pair(15.dp, 10.dp)
    }

    val presentationState = rememberPresentationState(exoPlayer)
    MainActivityViewConnectionMonitor(context)

    Box(
        modifier = Modifier
            .fillMaxSize()
            .background(Color.Black),
        contentAlignment = Alignment.Center
    ) {
        PlayerSurface(
            player = exoPlayer,
            surfaceType = SURFACE_TYPE_SURFACE_VIEW,
            modifier = Modifier
                .fillMaxSize()
                .resizeWithContentScale(ContentScale.Crop, presentationState.videoSizeDp)
        )

        if (isPortrait) {
            val columnScrollState = rememberScrollState()

            Column(
                Modifier
                    .padding(horizontal = rootPadding)
                    .verticalScroll(columnScrollState),
                horizontalAlignment = Alignment.CenterHorizontally,
            ) {
                TitleView(viewModel, Modifier)
                Spacer(Modifier.height(spacerSize - 5.dp))

                Row(
                    verticalAlignment = Alignment.CenterVertically,
                ) {
                    ConnectionStatusView(viewModel, Modifier.fillMaxWidth())
                }
                Spacer(Modifier.height(spacerSize - 5.dp))

                ConnectionInfoView(
                    viewModel,
                    Modifier.width(connInfoWidth)
                )

                Spacer(Modifier.height(spacerSize - 5.dp))
                Surface(
                    modifier = Modifier.padding(horizontal = 30.dp),
                    color = colorCardBackground,
                    shape = RoundedCornerShape(10.dp),
                    border = strokeCardBorder
                ) {
                    UpdateView(
                        viewModel,
                        modifier = Modifier
                            .width(updateViewWidth)
                            .padding(
                                horizontal = updateViewPaddingWidth,
                                vertical = updateViewPaddingHeight
                            )
                    )
                }
            }
        } else {
            Row(
                modifier = Modifier.padding(vertical = rootPadding),
                verticalAlignment = Alignment.CenterVertically
            ) {
                val leftColumnScrollState = rememberScrollState()
                val rightColumnScrollState = rememberScrollState()
                val columnPadding = when (getScreenSize((getScreenResolution()))) {
                    ScreenSize.Tiny -> 30.dp
                    ScreenSize.Small -> 30.dp
                    ScreenSize.Medium -> 60.dp
                    ScreenSize.Large -> 60.dp
                }

                Column(
                    modifier = Modifier
                        .padding(horizontal = columnPadding)
                        .weight(1f)
                        .verticalScroll(leftColumnScrollState),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    TitleView(viewModel, Modifier.fillMaxWidth())
                    Spacer(
                        Modifier
                            .fillMaxWidth()
                            .height(spacerSize)
                    )
                    ConnectionStatusView(viewModel, Modifier.fillMaxWidth())

                    Spacer(
                        Modifier
                            .fillMaxWidth()
                            .height(spacerSize * 2)
                    )
                    Surface(
                        modifier = Modifier.padding(horizontal = 30.dp),
                        color = colorCardBackground,
                        shape = RoundedCornerShape(10.dp),
                        border = strokeCardBorder
                    ) {
                        UpdateView(
                            viewModel,
                            modifier = Modifier
                                .width(updateViewWidth)
                                .padding(
                                    horizontal = updateViewPaddingWidth,
                                    vertical = updateViewPaddingHeight
                                )
                        )
                    }
                }
                Column(
                    modifier = Modifier
                        .padding(horizontal = columnPadding)
                        .weight(1f)
                        .verticalScroll(rightColumnScrollState),
                    horizontalAlignment = Alignment.CenterHorizontally
                ) {
                    ConnectionInfoView(viewModel, Modifier.width(connInfoWidth))
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
    viewModel.updateState = UpdateState.NoUpdateAvailable
    viewModel.textPorts = "46899 (TCP)"
    viewModel.showQR = true
    viewModel.qrSize = 165f
//    viewModel.updating = true

    viewModel.ipInfo = mutableStateListOf(
        NetworkInterfaceData(
            NetworkInterfaceType.Wired, "Ethernet", "123.456.7.890", null
        )
    )
//    viewModel.ipInfo = mutableStateListOf<NetworkInterfaceData>()

    MainActivity(viewModel)
}

@SuppressLint("UnrememberedMutableState")
@Preview
@Composable
fun MainActivityPortraitPreview() {
    val viewModel = MainActivityViewModel()
    viewModel.updateStatus = stringResource(R.string.update_status)
    viewModel.updateState = UpdateState.NoUpdateAvailable
    viewModel.textPorts = "46899 (TCP)"
    viewModel.showQR = true
    viewModel.qrSize = 90f
    viewModel.updateState = UpdateState.UpdateAvailable
    viewModel.ipInfo = mutableStateListOf(
        NetworkInterfaceData(
            NetworkInterfaceType.Wired, "Ethernet", "123.456.7.890", null
        )
    )
//    viewModel.ipInfo = mutableStateListOf<NetworkInterfaceData>()


    MainActivity(viewModel)
}
