package com.futo.fcast.receiver.models

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.media3.common.Player

enum class ControlFocus {
    None,
    ProgressBar,
    Action,
    PlayPrevious,
    PlayNext,
}

class PlayerActivityViewModel : ViewModel() {
    var exoPlayer by mutableStateOf<Player?>(null)
    var errorMessage by mutableStateOf<String?>(null)
    var showControls by mutableStateOf(false)

    var isLoading by mutableStateOf(false)
    var isIdle by mutableStateOf(true)
    var playMessage by mutableStateOf<PlayMessage?>(null)

    // Hide
    // [<<][>][>>]
    // [|<][>][>|]
    // Hide
    var controlFocus by mutableStateOf(ControlFocus.None)
}
