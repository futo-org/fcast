package com.futo.fcast.receiver.composables

import androidx.compose.animation.core.Animatable
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.animation.core.RepeatMode
import androidx.compose.animation.core.infiniteRepeatable
import androidx.compose.animation.core.tween
import androidx.compose.foundation.Canvas
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.size
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.remember
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.rotate
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.drawscope.Stroke
import androidx.compose.ui.tooling.preview.Preview
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.delay

@Composable
fun Spinner(modifier: Modifier = Modifier) {
    val rotation =
        remember { listOf(Animatable(0f), Animatable(0f), Animatable(0f), Animatable(0f)) }
    val delays = remember { listOf(360L, 240L, 120L, 0L) }

    repeat(delays.size) {
        LaunchedEffect(Unit) {
            delay(delays[it])

            rotation[it].animateTo(
                targetValue = 360f,
                animationSpec = infiniteRepeatable(
                    animation = tween(
                        durationMillis = 1200,
                        easing = CubicBezierEasing(0.5f, 0f, 0.5f, 1f)
                    ),
                    repeatMode = RepeatMode.Restart
                )
            )
        }
    }

    Box(
        modifier = modifier.size(140.dp),
        contentAlignment = Alignment.Center
    ) {
        repeat(delays.size) {
            Box(
                modifier = Modifier
                    .size(124.dp)
                    .rotate(rotation[it].value)
            ) {
                Canvas(
                    modifier = Modifier
                        .size(124.dp)
                        .align(Alignment.Center)
                ) {
                    val strokePx = 4.dp.toPx()
                    drawArc(
                        color = Color.White,
                        startAngle = 225f,
                        sweepAngle = 90f,
                        useCenter = false,
                        topLeft = Offset(strokePx / 2, strokePx / 2),
                        size = Size(this.size.width - strokePx, this.size.height - strokePx),
                        style = Stroke(width = strokePx, cap = StrokeCap.Butt)
                    )
                }
            }
        }
    }
}

@Preview
@Composable
fun SpinnerPreview() {
    Column(
        horizontalAlignment = Alignment.CenterHorizontally
    ) {
        Box(modifier = Modifier
            .fillMaxWidth()
            .height(40.dp)) {}
        Spinner()
    }
}
