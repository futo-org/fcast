package com.futo.fcast.receiver.composables

import androidx.compose.runtime.Composable
import androidx.compose.runtime.DisposableEffect
import androidx.compose.runtime.mutableLongStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.media3.common.Player
import androidx.compose.runtime.getValue
import androidx.compose.runtime.setValue

@Composable
fun rememberPlayerState(player: Player): PlayerState {
    var currentPosition by remember { mutableLongStateOf(0L) }
    var duration by remember { mutableLongStateOf(0L) }
    var isPlaying by remember { mutableStateOf(false) }

    DisposableEffect(player) {
        val listener = object : Player.Listener {
            override fun onEvents(player: Player, events: Player.Events) {
                currentPosition = player.currentPosition
                duration = player.duration
                isPlaying = player.isPlaying
            }
        }

        player.addListener(listener)

        onDispose {
            player.removeListener(listener)
        }
    }

    return remember(currentPosition, duration, isPlaying) {
        PlayerState(currentPosition, duration, isPlaying)
    }
}

data class PlayerState(
    val currentPosition: Long,
    val duration: Long,
    val isPlaying: Boolean
)
