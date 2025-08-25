package com.futo.fcast.receiver.composables

import androidx.compose.foundation.clickable
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.runtime.Composable
import androidx.compose.runtime.remember
import androidx.compose.ui.Modifier

@Composable
fun Modifier.noRippleClickable(onClick: () -> Unit): Modifier =
    clickable(
        interactionSource = remember { MutableInteractionSource() },
        indication = null, // to prevent the ripple from the tap
    ) {
        onClick()
    }