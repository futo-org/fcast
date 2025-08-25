package com.futo.fcast.receiver.composables

import android.content.Context
import android.widget.Toast
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableIntStateOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.setValue
import com.futo.fcast.receiver.R
import com.futo.fcast.receiver.models.MainActivityViewModel
import com.futo.fcast.receiver.models.PlayerActivityViewModel

@Composable
fun MainActivityViewConnectionMonitor(viewModel: MainActivityViewModel, context: Context) {
    var connectionsLastSize by remember { mutableIntStateOf(viewModel.connections.size) }

    LaunchedEffect(key1 = viewModel.connections.size) {
        if (connectionsLastSize > viewModel.connections.size) {
            val textResource = if (viewModel.connections.isEmpty()) R.string.main_device_disconnected else R.string.main_device_disconnected_multiple
            Toast.makeText(context, context.getString(textResource), Toast.LENGTH_LONG).show()
        }

        connectionsLastSize = viewModel.connections.size
    }
}

// todo fix player activity toasts conflict with main activity
@Composable
fun PlayerActivityViewConnectionMonitor(viewModel: PlayerActivityViewModel, context: Context) {
    var initialUpdate by remember { mutableStateOf(true) }
    var connectionsLastSize by remember { mutableIntStateOf(viewModel.connections.size) }

    LaunchedEffect(key1 = viewModel.connections.size) {
        if (connectionsLastSize > viewModel.connections.size) {
            Toast.makeText(context, context.getString(R.string.player_device_disconnected), Toast.LENGTH_LONG).show()
        }
        else {
            if (!initialUpdate) {
                Toast.makeText(context, context.getString(R.string.player_device_connected), Toast.LENGTH_LONG).show()
            }
            else {
                initialUpdate = false
            }
        }

        connectionsLastSize = viewModel.connections.size
    }
}
