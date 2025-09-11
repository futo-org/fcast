package com.futo.fcast.receiver.composables

import androidx.compose.foundation.BorderStroke
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import com.futo.fcast.receiver.R

val colorCardBackground = Color(0x80141414)
val colorPrimary = Color(0xFF019BE7)
val colorButtonPrimary = Color(0xFF008BD7)
val colorButtonSecondary = Color(0xFF3E3E3E)

val outfitFontFamilyExtraBold = FontFamily(Font(R.font.outfit_extra_bold))
val interFontFamily = FontFamily(
    Font(R.font.inter_light, FontWeight.Light),
    Font(R.font.inter_regular, FontWeight.Normal),
    Font(R.font.inter_bold, FontWeight.Bold)
)

val strokeCardBorder = BorderStroke(1.dp, Color(0xFF2E2E2E))
