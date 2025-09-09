package com.futo.fcast.receiver.composables

import android.content.Context
import android.widget.Toast
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import androidx.lifecycle.Lifecycle
import androidx.lifecycle.compose.LocalLifecycleOwner
import com.futo.fcast.receiver.R
import java.util.UUID

var frontendConnections = mutableStateListOf<UUID>()

@Composable
fun MainActivityViewConnectionMonitor(context: Context) {
    val lifecycleOwner = LocalLifecycleOwner.current
    var connectionsLastSize by remember { mutableIntStateOf(frontendConnections.size) }

    LaunchedEffect(key1 = frontendConnections.size) {
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED) && connectionsLastSize > frontendConnections.size) {
            val textResource = if (frontendConnections.isEmpty()) R.string.main_device_disconnected else R.string.main_device_disconnected_multiple
            Toast.makeText(context, context.getString(textResource), Toast.LENGTH_LONG).show()
        }

        connectionsLastSize = frontendConnections.size
    }
}

@Composable
fun PlayerActivityViewConnectionMonitor(context: Context) {
    val lifecycleOwner = LocalLifecycleOwner.current
    var initialUpdate by remember { mutableStateOf(true) }
    var connectionsLastSize by remember { mutableIntStateOf(frontendConnections.size) }

    LaunchedEffect(key1 = frontendConnections.size) {
        if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED) && connectionsLastSize > frontendConnections.size) {
            Toast.makeText(context, context.getString(R.string.player_device_disconnected), Toast.LENGTH_LONG).show()
        }
        else {
            if (lifecycleOwner.lifecycle.currentState.isAtLeast(Lifecycle.State.RESUMED) && !initialUpdate) {
                Toast.makeText(context, context.getString(R.string.player_device_connected), Toast.LENGTH_LONG).show()
            }
            else {
                initialUpdate = false
            }
        }

        connectionsLastSize = frontendConnections.size
    }
}
