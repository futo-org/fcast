package com.futo.fcast.receiver.composables

import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay

@Composable
fun DelayedLoadingIndicator(
    modifier: Modifier,
    delayMs: Long = 100
) {
    var showIndicator by remember { mutableStateOf(false) }
    LaunchedEffect(Unit) {
        delay(delayMs)
        showIndicator = true
    }

    if (showIndicator) {
        CircularProgressIndicator(
            modifier = modifier,
            color = Color.White,
            strokeWidth = 4.dp
        )
    }
}
