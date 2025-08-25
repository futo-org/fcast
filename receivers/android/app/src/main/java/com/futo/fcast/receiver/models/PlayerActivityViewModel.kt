package com.futo.fcast.receiver.models

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import java.util.UUID

class PlayerActivityViewModel : ViewModel() {
    var statusMessage by mutableStateOf<String?>(null)
    var showControls by mutableStateOf(false)
    var connections = mutableStateListOf<UUID>()
}
