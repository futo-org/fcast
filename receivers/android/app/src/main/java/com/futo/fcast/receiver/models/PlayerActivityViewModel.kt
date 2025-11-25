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
    Settings,
    SettingsDialog,
}

enum class SettingsDialogMenuType {
    None,
    Settings,
    Subtitles,
    PlaybackSpeed,
}

val playbackSpeeds = listOf("0.25", "0.50", "0.75", "1.00", "1.25", "1.50", "1.75", "2.00")

sealed class PlayerSource {
    data class Exo(val exoPlayer: Player) : PlayerSource()
    data class Whep(
        val client: WhepClient,
        var videoTrack: MutableState<VideoTrack?> = mutableStateOf(null),
        var surfaceIsInit: MutableState<Boolean> = mutableStateOf(false)
    ) : PlayerSource()
}

class PlayerActivityViewModel : ViewModel() {
    var errorMessage by mutableStateOf<String?>(null)
    var showControls by mutableStateOf(false)
    var source by mutableStateOf<PlayerSource?>(null)
    var showSettingsDialog by mutableStateOf(false)
    var showPlaybackSpeedSettingsDialog by mutableStateOf(false)
    var showSubtitlesSettingsDialog by mutableStateOf(false)

    var isLoading by mutableStateOf(false)
    var isIdle by mutableStateOf(true)
    var playMessage by mutableStateOf<PlayMessage?>(null)
    var hasDuration by mutableStateOf(true)
    var subtitles by mutableStateOf(listOf("Off"))

    // TODO: Migrate to multidimensional array instead of using conditionals
    // Hide
    // [<<][>][>>]
    // [|<][>][>|]
    // Hide
    var controlFocus by mutableStateOf(ControlFocus.None)
    var settingsControlFocus by mutableStateOf(Pair(SettingsDialogMenuType.None, 0))

    fun hideAllSettingDialogs() {
        showSettingsDialog = false
        showSubtitlesSettingsDialog = false
        showPlaybackSpeedSettingsDialog = false
        settingsControlFocus = Pair(SettingsDialogMenuType.None, 0)
    }

    fun toggleSettingsDialog() {
        if (showSettingsDialog) {
            hideAllSettingDialogs()
        } else {
            showSettingsDialog()
        }
    }

    fun showSettingsDialog() {
        showSettingsDialog = true
        showSubtitlesSettingsDialog = false
        showPlaybackSpeedSettingsDialog = false
    }

    fun showSubtitlesSettingsDialog() {
        showSettingsDialog = false
        showSubtitlesSettingsDialog = true
        showPlaybackSpeedSettingsDialog = false
    }

    fun showPlaybackSpeedSettingsDialog() {
        showSettingsDialog = false
        showSubtitlesSettingsDialog = false
        showPlaybackSpeedSettingsDialog = true
    }
}
