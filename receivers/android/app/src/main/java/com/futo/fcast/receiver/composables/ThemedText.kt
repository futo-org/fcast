package com.futo.fcast.receiver.composables

import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.text.style.TextAlign
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.TextUnit

@Composable
fun ThemedText(
    text: String,
    modifier: Modifier = Modifier,
    fontSize: TextUnit = getDefaultFontSize(),
    fontWeight: FontWeight = FontWeight.Normal
) {
    Text(
        text = text,
        modifier = modifier,
        color = Color.White,
        fontSize = fontSize,
        fontFamily = interFontFamily,
        fontWeight = fontWeight,
        textAlign = TextAlign.Center,
        overflow = TextOverflow.Ellipsis
    )
}
