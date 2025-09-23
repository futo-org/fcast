package com.futo.fcast.receiver.models

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableFloatStateOf
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.lifecycle.ViewModel
import com.futo.fcast.receiver.MainActivity
import com.futo.fcast.receiver.NetworkInterfaceData

enum class UpdateState {
    NoUpdateAvailable,
    UpdateAvailable,
    Downloading,
    Installing,
    InstallSuccess,
    InstallFailure,
}

class MainActivityViewModel : ViewModel() {
    var showQR by mutableStateOf(true)
    var qrSize by mutableFloatStateOf(0f)
    var imageQR by mutableStateOf<ImageBitmap?>(null)
    var textPorts by mutableStateOf("")
    var updateState by mutableStateOf(UpdateState.NoUpdateAvailable)
    var updateStatus by mutableStateOf("")
    var updateProgress by mutableFloatStateOf(0f)

    var ipInfo = mutableStateListOf<NetworkInterfaceData>()

    fun update() {
        MainActivity.instance?.update()
    }
}