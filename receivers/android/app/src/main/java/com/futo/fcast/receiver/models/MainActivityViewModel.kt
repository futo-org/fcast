package com.futo.fcast.receiver.models

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateListOf
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue
import androidx.compose.ui.graphics.ImageBitmap
import androidx.lifecycle.ViewModel
import com.futo.fcast.receiver.MainActivity
import com.futo.fcast.receiver.NetworkInterfaceData
import java.util.UUID

class MainActivityViewModel : ViewModel() {
    var showQR by mutableStateOf(true)
    var imageQR by mutableStateOf<ImageBitmap?>(null)
    var textPorts by mutableStateOf("")
    var updateStatus by mutableStateOf<String?>("")
    var updateAvailable by mutableStateOf(false)
    var updating by mutableStateOf(false)
    var updateProgress by mutableStateOf("")
    var updateResultSuccessful by mutableStateOf<Boolean?>(null)

    var ipInfo = mutableStateListOf<NetworkInterfaceData>()

    fun update() { MainActivity.instance?.update() }
}