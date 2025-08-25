package com.futo.fcast.receiver.composables

import androidx.annotation.DrawableRes
import androidx.compose.animation.graphics.res.animatedVectorResource
import androidx.compose.animation.graphics.res.rememberAnimatedVectorPainter
import androidx.compose.animation.graphics.vector.AnimatedImageVector
import androidx.compose.foundation.Image
import androidx.compose.foundation.layout.Column
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.tooling.preview.Preview
import com.futo.fcast.receiver.R

@Composable
fun Spinner(modifier: Modifier, @DrawableRes id: Int) {
    val image = AnimatedImageVector.animatedVectorResource(id)
    var atEnd by remember { mutableStateOf(false) }

    LaunchedEffect(Unit) {
        atEnd = true
    }

    Image(
        painter = rememberAnimatedVectorPainter(image, atEnd),
        contentDescription = null,
        modifier = modifier
    )
}

@Preview
@Composable
fun SpinnerPreview() {
    Column {
        Spinner(Modifier, R.drawable.ic_loader_animated)
        Spinner(Modifier, R.drawable.ic_update_animated)
    }
}

