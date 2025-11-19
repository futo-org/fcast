package com.futo.fcast.receiver.models

import androidx.compose.runtime.MutableState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.lifecycle.ViewModel
import androidx.media3.common.Player
import com.futo.fcast.receiver.WhepClient
import org.webrtc.VideoTrack

enum class ControlFocus {
    None,
    ProgressBar,
    Action,
    PlayPrevious,
    PlayNext,
    SeekForward,
    SeekBackward,
}

sealed class PlayerSource {
    data class Exo(val exoPlayer: Player): PlayerSource()
    data class Whep(
        val client: WhepClient,
        var videoTrack: MutableState<VideoTrack?> = mutableStateOf(null),
        var surfaceIsInit: MutableState<Boolean> = mutableStateOf(false)
    ): PlayerSource()
}

class PlayerActivityViewModel : ViewModel() {
    var errorMessage by mutableStateOf<String?>(null)
    var showControls by mutableStateOf(false)
    var source by mutableStateOf<PlayerSource?>(null)

    var isLoading by mutableStateOf(false)
    var isIdle by mutableStateOf(true)
    var playMessage by mutableStateOf<PlayMessage?>(null)
    var hasDuration by mutableStateOf(true)

    // Hide
    // [<<][>][>>]
    // [|<][>][>|]
    // Hide
    var controlFocus by mutableStateOf(ControlFocus.None)
}
