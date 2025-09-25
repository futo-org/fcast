package com.futo.fcast.receiver

import android.Manifest
import android.annotation.SuppressLint
import android.app.AlertDialog
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.util.Base64
import android.util.Log
import android.util.TypedValue
import android.view.KeyEvent
import android.view.WindowManager
import android.widget.Toast
import androidx.activity.compose.setContent
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.annotation.OptIn
import androidx.appcompat.app.AppCompatActivity
import androidx.compose.ui.graphics.asImageBitmap
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.core.net.toUri
import androidx.lifecycle.lifecycleScope
import androidx.media3.common.MediaItem
import androidx.media3.common.Player
import androidx.media3.common.util.UnstableApi
import androidx.media3.exoplayer.ExoPlayer
import com.futo.fcast.receiver.composables.getQrSize
import com.futo.fcast.receiver.composables.getScreenResolution
import com.futo.fcast.receiver.models.EventMessage
import com.futo.fcast.receiver.models.EventType
import com.futo.fcast.receiver.models.FCastNetworkConfig
import com.futo.fcast.receiver.models.FCastService
import com.futo.fcast.receiver.models.MainActivityViewModel
import com.futo.fcast.receiver.models.UpdateState
import com.futo.fcast.receiver.views.MainActivity
import com.google.zxing.BarcodeFormat
import com.google.zxing.EncodeHintType
import com.journeyapps.barcodescanner.BarcodeEncoder
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.launch
import kotlinx.coroutines.withContext
import kotlinx.serialization.json.Json
import okhttp3.OkHttpClient
import java.io.InputStream
import java.io.OutputStream


class MainActivity : AppCompatActivity() {
    private lateinit var _player: ExoPlayer
    private lateinit var _systemAlertWindowPermissionLauncher: ActivityResultLauncher<Intent>
    private val _preferenceFileKey get() = "$packageName.PREFERENCE_FILE_KEY"

    val viewModel = MainActivityViewModel()
    private var _screenResolution: Pair<Int, Int> = Pair(0, 0)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)

        _player = ExoPlayer.Builder(this).build()
        setContent {
            _screenResolution = getScreenResolution()
            MainActivity(viewModel, _player)
        }


        _systemAlertWindowPermissionLauncher =
            registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { _ ->
                if (Settings.canDrawOverlays(this)) {
                    // Permission granted, you can launch the activity from the foreground service
                    Toast.makeText(this, "Alert window permission granted", Toast.LENGTH_LONG)
                        .show()
                    Log.i(TAG, "Alert window permission granted")
                } else {
                    // Permission denied, notify the user and request again if necessary
                    Toast.makeText(
                        this,
                        "Permission is required to work in background",
                        Toast.LENGTH_LONG
                    ).show()
                    Log.i(TAG, "Alert window permission denied")
                }
            }

        if (savedInstanceState != null && savedInstanceState.containsKey("updateAvailable")) {
            val ordinalValue =
                savedInstanceState.getInt("updateAvailable", UpdateState.NoUpdateAvailable.ordinal)
            viewModel.updateState = UpdateState.entries.toTypedArray()[ordinalValue]
        }

        startVideo()
        viewModel.updateStatus = getString(R.string.checking_for_updates)

        if (!BuildConfig.IS_PLAYSTORE_VERSION) {
            lifecycleScope.launch(Dispatchers.IO) {
                checkForUpdates()
            }
        }

        renderIPsAndQRCode()
        instance = this
        NetworkService.activityCount++

        checkAndRequestPermissions()
        if (savedInstanceState == null && NetworkService.instance == null) {
            restartService()
        }

        requestSystemAlertWindowPermission()
    }

    override fun onPause() {
        super.onPause()
        _player.playWhenReady = false
        _player.pause()
    }

    override fun onResume() {
        super.onResume()
        _player.playWhenReady = true
        _player.play()
    }

    override fun onDestroy() {
        super.onDestroy()
        instance = null
        InstallReceiver.onReceiveResult = null
        _player.release()
        NetworkService.activityCount--
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        outState.putInt("updateAvailable", viewModel.updateState.ordinal)
    }

    override fun onRequestPermissionsResult(
        requestCode: Int,
        permissions: Array<out String>,
        grantResults: IntArray
    ) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)

        when (requestCode) {
            REQUEST_ID_MULTIPLE_PERMISSIONS -> {
                val perms: MutableMap<String, Int> = HashMap()
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    perms[Manifest.permission.POST_NOTIFICATIONS] =
                        PackageManager.PERMISSION_GRANTED
                }

                if (grantResults.isNotEmpty()) {
                    var i = 0
                    while (i < permissions.size) {
                        perms[permissions[i]] = grantResults[i]
                        i++
                    }

                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                        if (perms[Manifest.permission.POST_NOTIFICATIONS] == PackageManager.PERMISSION_GRANTED) {
                            Log.i(TAG, "Notification permission granted")
                            Toast.makeText(
                                this,
                                "Notification permission granted",
                                Toast.LENGTH_LONG
                            ).show()
                            restartService()
                        } else {
                            Log.i(TAG, "Notification permission not granted")
                            Toast.makeText(
                                this,
                                "App may not fully work without notification permission",
                                Toast.LENGTH_LONG
                            ).show()
                            restartService()
                        }
                    }
                }
            }
        }
    }

    @SuppressLint("GestureBackNavigation")
    @OptIn(UnstableApi::class)
    override fun dispatchKeyEvent(event: KeyEvent): Boolean {
//        Log.d(TAG, "KeyEvent: label=${event.displayLabel}, event=$event")
//        var handledCase = false
        var key = event.displayLabel.toString()

        if (event.action == KeyEvent.ACTION_DOWN) {
            when (event.keyCode) {
                KeyEvent.KEYCODE_DPAD_CENTER -> key = "Enter"
                KeyEvent.KEYCODE_DPAD_UP -> key = "ArrowUp"
                KeyEvent.KEYCODE_DPAD_DOWN -> key = "ArrowDown"
                KeyEvent.KEYCODE_DPAD_LEFT -> key = "ArrowLeft"
                KeyEvent.KEYCODE_DPAD_RIGHT -> key = "ArrowRight"
                KeyEvent.KEYCODE_MEDIA_STOP -> key = "Stop"
                KeyEvent.KEYCODE_MEDIA_REWIND -> key = "Rewind"
                KeyEvent.KEYCODE_MEDIA_PLAY -> key = "Play"
                KeyEvent.KEYCODE_MEDIA_PAUSE -> key = "Pause"
                KeyEvent.KEYCODE_MEDIA_FAST_FORWARD -> key = "FastForward"
                KeyEvent.KEYCODE_BACK -> key = "Back"
            }
        }

        if (NetworkService.instance?.getSubscribedKeys()?.first?.contains(key) == true) {
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    com.futo.fcast.receiver.models.KeyEvent(
                        EventType.KeyDown,
                        key,
                        event.repeatCount > 0,
//                    handledCase
                        true
                    )
                )
            )
        }
        if (NetworkService.instance?.getSubscribedKeys()?.second?.contains(key) == true) {
            NetworkService.instance?.sendEvent(
                EventMessage(
                    System.currentTimeMillis(),
                    com.futo.fcast.receiver.models.KeyEvent(
                        EventType.KeyUp,
                        key,
                        event.repeatCount > 0,
//                    handledCase
                        true
                    )
                )
            )
        }

//        if (handledCase) {
//            return true
//        }

        return super.dispatchKeyEvent(event)
    }

    fun networkChanged() {
        NetworkService.instance?.discoveryService?.stop()
        NetworkService.instance?.discoveryService?.start()

        renderIPsAndQRCode()
    }

    private fun renderIPsAndQRCode() {
        val ipInfo = NetworkService.instance?.networkWorker?.interfaces ?: listOf()
        val ips = ipInfo.map { it.address }
        viewModel.ipInfo.clear()
        viewModel.ipInfo.addAll(ipInfo)
        viewModel.textPorts =
            "${TcpListenerService.PORT} (TCP), ${WebSocketListenerService.PORT} (WS)"

        val qrSize = getQrSize(_screenResolution)
        viewModel.qrSize = qrSize
        Log.i(TAG, "QR code size: $qrSize")

        try {
            val barcodeEncoder = BarcodeEncoder()
            val px = TypedValue.applyDimension(
                TypedValue.COMPLEX_UNIT_DIP,
                qrSize,
                resources.displayMetrics
            ).toInt()
            val hints = mapOf(EncodeHintType.MARGIN to 1)
            val json = Json.encodeToString(
                FCastNetworkConfig(
                    "${Build.MANUFACTURER}-${Build.MODEL}", ips, listOf(
                        FCastService(TcpListenerService.PORT, 0),
                        FCastService(WebSocketListenerService.PORT, 1)
                    )
                )
            )
            val base64 = Base64.encodeToString(
                json.toByteArray(),
                Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP
            )
            val url = "fcast://r/${base64}"
            Log.i(TAG, "connection url: $url")
            val bitmap = barcodeEncoder.encodeBitmap(url, BarcodeFormat.QR_CODE, px, px, hints)
            viewModel.imageQR = bitmap.asImageBitmap()
        } catch (e: Exception) {
            viewModel.showQR = false

            Log.e(TAG, "Error generating QR code: ${e.message}")
            Toast.makeText(
                this,
                this.getString(R.string.qr_code_error),
                Toast.LENGTH_LONG
            ).show()
        }
    }

    private fun restartService() {
        NetworkService.instance?.stopSelf()
        startService(Intent(this, NetworkService::class.java))
    }

    private fun startVideo() {
        val mediaItem =
            MediaItem.fromUri(("android.resource://" + packageName + "/" + R.raw.c).toUri())
        _player.setMediaItem(mediaItem)
        _player.prepare()
        _player.repeatMode = Player.REPEAT_MODE_ALL
        _player.playWhenReady = true
    }

    private fun checkAndRequestPermissions(): Boolean {
        val listPermissionsNeeded = arrayListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val notificationPermission =
                ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
            if (notificationPermission != PackageManager.PERMISSION_GRANTED) {
                listPermissionsNeeded.add(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        if (listPermissionsNeeded.isNotEmpty()) {
            val permissionRequestedKey = "NOTIFICATIONS_PERMISSION_REQUESTED"
            val sharedPref = this.getSharedPreferences(_preferenceFileKey, MODE_PRIVATE)
            val hasRequestedPermission = sharedPref.getBoolean(permissionRequestedKey, false)
            if (!hasRequestedPermission) {
                ActivityCompat.requestPermissions(
                    this,
                    listPermissionsNeeded.toTypedArray(),
                    REQUEST_ID_MULTIPLE_PERMISSIONS
                )
                with(sharedPref.edit()) {
                    putBoolean(permissionRequestedKey, true)
                    apply()
                }
            } else {
                Toast.makeText(this, "Notifications permission missing", Toast.LENGTH_SHORT).show()
            }
            return false
        }

        return true
    }

    private fun requestSystemAlertWindowPermission() {
        try {
            val permissionRequestedKey = "SYSTEM_ALERT_WINDOW_PERMISSION_REQUESTED"
            val sharedPref = this.getSharedPreferences(_preferenceFileKey, MODE_PRIVATE)
            val hasRequestedPermission = sharedPref.getBoolean(permissionRequestedKey, false)

            if (!Settings.canDrawOverlays(this)) {
                if (!hasRequestedPermission) {
                    AlertDialog.Builder(this)
                        .setTitle(R.string.permission_dialog_title)
                        .setMessage(R.string.permission_dialog_message)
                        .setPositiveButton(R.string.permission_dialog_positive_button) { _, _ ->
                            try {
                                val intent = Intent(
                                    Settings.ACTION_MANAGE_OVERLAY_PERMISSION,
                                    "package:$packageName".toUri()
                                )
                                _systemAlertWindowPermissionLauncher.launch(intent)
                            } catch (e: Throwable) {
                                Log.e("OverlayPermission", "Error requesting overlay permission", e)
                                Toast.makeText(
                                    this,
                                    "An error occurred: ${e.message}",
                                    Toast.LENGTH_LONG
                                ).show()
                            }
                        }
                        .setNegativeButton(R.string.permission_dialog_negative_button) { dialog, _ ->
                            dialog.dismiss()
                            Toast.makeText(
                                this,
                                "Permission is required to work in background",
                                Toast.LENGTH_LONG
                            ).show()
                        }
                        .create()
                        .show()

                    with(sharedPref.edit()) {
                        putBoolean(permissionRequestedKey, true)
                        apply()
                    }
                } else {
                    Toast.makeText(
                        this,
                        "Optional system alert window permission missing",
                        Toast.LENGTH_SHORT
                    ).show()
                }
            }
        } catch (_: Throwable) {
            Log.e(TAG, "Failed to request system alert window permissions")
        }
    }

    private suspend fun checkForUpdates() {
        Log.i(TAG, "Checking for updates...")

        withContext(Dispatchers.IO) {
            try {
                val latestVersion = downloadVersionCode()

                if (latestVersion != null) {
                    val currentVersion = BuildConfig.VERSION_CODE
                    Log.i(TAG, "Current version $currentVersion latest version $latestVersion.")

                    withContext(Dispatchers.Main) {
                        setUpdateAvailable(latestVersion > currentVersion)
                    }
                } else {
                    Log.w(TAG, "Failed to retrieve version from version URL.")

                    withContext(Dispatchers.Main) {
                        Toast.makeText(
                            this@MainActivity,
                            "Failed to retrieve version",
                            Toast.LENGTH_LONG
                        ).show()
                    }
                }
            } catch (e: Throwable) {
                Log.w(TAG, "Failed to check for updates.", e)

                withContext(Dispatchers.Main) {
                    Toast.makeText(
                        this@MainActivity,
                        "Failed to check for updates",
                        Toast.LENGTH_LONG
                    ).show()
                }
            }
        }
    }

    private fun setUpdateAvailable(updateAvailable: Boolean) {
        if (updateAvailable) {
            viewModel.updateStatus = getString(R.string.update_status)
            viewModel.updateState = UpdateState.UpdateAvailable
        } else {
            viewModel.updateStatus = ""
            viewModel.updateState = UpdateState.NoUpdateAvailable
        }
    }

    private fun downloadVersionCode(): Int? {
        val client = OkHttpClient()
        val request = okhttp3.Request.Builder()
            .method("GET", null)
            .url(VERSION_URL)
            .build()

        val response = client.newCall(request).execute()
        if (!response.isSuccessful) {
            return null
        }

        return response.body.string().trim().toInt()
    }

    fun update() {
        viewModel.updateState = UpdateState.Downloading
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
        viewModel.updateStatus = getString(R.string.downloading_update)

        lifecycleScope.launch(Dispatchers.IO) {
            var inputStream: InputStream? = null
            try {
                val client = OkHttpClient()
                val request = okhttp3.Request.Builder()
                    .method("GET", null)
                    .url(APK_URL)
                    .build()

                val response = client.newCall(request).execute()
                val body = response.body
                if (response.isSuccessful) {
                    inputStream = body.byteStream()
                    val dataLength = body.contentLength()
                    install(inputStream, dataLength)
                } else {
                    throw Exception("Failed to download latest version of app.")
                }
            } catch (e: Throwable) {
                Log.w(
                    TAG,
                    "Exception thrown while downloading and installing latest version of app.",
                    e
                )
                withContext(Dispatchers.Main) {
                    onReceiveResult("Failed to download update.")
                }
            } finally {
                inputStream?.close()
            }
        }
    }

    private suspend fun install(inputStream: InputStream, dataLength: Long) {
        var lastProgressInt = 0
        var session: PackageInstaller.Session? = null

        try {
            Log.i(TAG, "Hooked InstallReceiver.onReceiveResult.")
            InstallReceiver.onReceiveResult = { message -> onReceiveResult(message) }

            val packageInstaller: PackageInstaller = packageManager.packageInstaller
            val params =
                PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
            val sessionId = packageInstaller.createSession(params)
            session = packageInstaller.openSession(sessionId)

            session.openWrite("package", 0, dataLength).use { sessionStream ->
                inputStream.copyToOutputStream(dataLength, sessionStream) { progress ->
                    val progressInt = (progress * 100.0f).toInt()
                    if (lastProgressInt != progressInt) {
                        lastProgressInt = progressInt

                        lifecycleScope.launch(Dispatchers.Main) {
                            viewModel.updateProgress = progress
                        }
                    }
                }

                session.fsync(sessionStream)
            }

            val intent = Intent(this, InstallReceiver::class.java)
            val pendingIntent = PendingIntent.getBroadcast(
                this,
                0,
                intent,
                PendingIntent.FLAG_MUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            )
            val statusReceiver = pendingIntent.intentSender

            session.commit(statusReceiver)
            session.close()

            withContext(Dispatchers.Main) {
                viewModel.updateProgress = 1f
                viewModel.updateStatus = getString(R.string.installing_update)
                viewModel.updateState = UpdateState.Installing
            }
        } catch (e: Throwable) {
            Log.w(
                TAG,
                "Exception thrown while downloading and installing latest version of app.",
                e
            )
            session?.abandon()
            withContext(Dispatchers.Main) {
                onReceiveResult("Failed to download update.")
            }
        } finally {
            withContext(Dispatchers.Main) {
                window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            }
        }
    }

    private fun onReceiveResult(result: String?) {
        InstallReceiver.onReceiveResult = null
        Log.i(TAG, "Cleared InstallReceiver.onReceiveResult handler.")

        if (result.isNullOrBlank()) {
            viewModel.updateState = UpdateState.InstallSuccess
            viewModel.updateStatus = getString(R.string.success)
        } else {
            viewModel.updateState = UpdateState.InstallFailure
            viewModel.updateStatus = result

            AlertDialog.Builder(this)
                .setTitle(getString(R.string.update_error))
                .setIcon(android.R.drawable.ic_dialog_alert)
                .setMessage(result)
                .setPositiveButton("OK") { dialog, _ ->
                    dialog.dismiss()
                }
                .show()
        }
    }

    private fun InputStream.copyToOutputStream(
        inputStreamLength: Long,
        outputStream: OutputStream,
        onProgress: (Float) -> Unit
    ) {
        val buffer = ByteArray(16384)
        var n: Int
        var total = 0
        val inputStreamLengthFloat = inputStreamLength.toFloat()

        while (read(buffer).also { n = it } >= 0) {
            total += n
            outputStream.write(buffer, 0, n)
            onProgress.invoke(total.toFloat() / inputStreamLengthFloat)
        }
    }

    companion object {
        var instance: MainActivity? = null

        private const val TAG = "MainActivity"
        private val VERSION_URL =
            if (BuildConfig.DEBUG) "https://dl.fcast.org/dev/unstable/android/fcast-version.txt" else "https://dl.fcast.org/android/fcast-version.txt"
        private val APK_URL =
            if (BuildConfig.DEBUG) "https://dl.fcast.org/dev/unstable/android/app-defaultFlavor-debug.apk" else "https://dl.fcast.org/android/fcast-release.apk"
        private const val REQUEST_ID_MULTIPLE_PERMISSIONS = 1
    }
}
