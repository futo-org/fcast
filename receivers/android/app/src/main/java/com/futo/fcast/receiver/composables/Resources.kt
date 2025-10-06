package com.futo.fcast.receiver.composables

import android.app.UiModeManager
import android.content.Context
import android.content.pm.PackageManager
import android.content.res.Configuration
import androidx.compose.foundation.BorderStroke
import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.platform.LocalConfiguration
import androidx.compose.ui.platform.LocalDensity
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.TextUnit
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import com.futo.fcast.receiver.R
import kotlin.math.max
import kotlin.math.min

val colorCardBackground = Color(0x80141414)
val colorPrimary = Color(0xFF019BE7)
val colorButtonPrimary = Color(0xFF008BD7)
val colorButtonSecondary = Color(0xFF3E3E3E)
val colorLive = Color(0xFFFB2C2C)

val outfitFontFamilyExtraBold = FontFamily(Font(R.font.outfit_extra_bold))
val interFontFamily = FontFamily(
    Font(R.font.inter_light, FontWeight.Light),
    Font(R.font.inter_regular, FontWeight.Normal),
    Font(R.font.inter_bold, FontWeight.Bold)
)

val strokeCardBorder = BorderStroke(1.dp, Color(0xFF2E2E2E))

enum class ScreenSize {
    Tiny, // 720p
    Small, // 1080p
    Medium, // 1440p
    Large, // 4K
}

@Composable
fun getDefaultFontSize(): TextUnit {
    return when (getScreenSize(getScreenResolution())) {
        ScreenSize.Tiny -> 11.sp
        ScreenSize.Small -> 11.sp
        ScreenSize.Medium -> 14.sp
        ScreenSize.Large -> 14.sp
    }
}

@Composable
fun getDefaultSpacerHeight(): Dp {
    return when (getScreenSize(getScreenResolution())) {
        ScreenSize.Tiny -> 15.dp
        ScreenSize.Small -> 15.dp
        ScreenSize.Medium -> 20.dp
        ScreenSize.Large -> 20.dp
    }
}

@Composable
fun getScreenResolution(): Pair<Int, Int> {
    val configuration = LocalConfiguration.current
    val density = LocalDensity.current
    val result = with(density) {
        Pair(
            configuration.screenWidthDp.dp.toPx().toInt(),
            configuration.screenHeightDp.dp.toPx().toInt()
        )
    }

    return result
}

fun getScreenSize(resolution: Pair<Int, Int>): ScreenSize {
    val long = max(resolution.first, resolution.second)
    val short = min(resolution.first, resolution.second)

    if (long >= 2560 || short >= 1440) {
        return ScreenSize.Large
    }
    if ((long >= 1920 && long < 2560) || (short >= 1080 && short < 1440)) {
        return ScreenSize.Medium
    }
    if ((long >= 1280 && long < 1920) || (short >= 720 && short < 1080)) {
        return ScreenSize.Small
    }
//    if (long < 1280 || short < 720) {
//        qrSize = 140f
//    }
    return ScreenSize.Tiny
}

fun getQrSize(resolution: Pair<Int, Int>): Float {
    return when (getScreenSize(resolution)) {
        ScreenSize.Tiny -> 120f
        ScreenSize.Small -> 130f
        ScreenSize.Medium -> 160f
        ScreenSize.Large -> 170f
    }
}

fun isAndroidTV(context: Context): Boolean {
    val uiModeManager = context.getSystemService(Context.UI_MODE_SERVICE) as? UiModeManager
    return uiModeManager?.currentModeType == Configuration.UI_MODE_TYPE_TELEVISION ||
            context.packageManager.hasSystemFeature(PackageManager.FEATURE_LEANBACK)
}
