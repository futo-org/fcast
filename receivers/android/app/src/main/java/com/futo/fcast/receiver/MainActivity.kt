package com.futo.fcast.receiver

import WebSocketListenerService
import android.Manifest
import android.app.Activity
import android.app.AlertDialog
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.graphics.drawable.Animatable
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.util.Base64
import android.util.Log
import android.util.TypedValue
import android.view.View
import android.view.WindowManager
import android.widget.*
import androidx.activity.result.ActivityResultLauncher
import androidx.activity.result.contract.ActivityResultContracts
import androidx.appcompat.app.AppCompatActivity
import androidx.constraintlayout.widget.ConstraintLayout
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import androidx.lifecycle.lifecycleScope
import androidx.media3.common.MediaItem
import androidx.media3.common.Player
import androidx.media3.exoplayer.ExoPlayer
import androidx.media3.ui.PlayerView
import com.google.zxing.BarcodeFormat
import com.journeyapps.barcodescanner.BarcodeEncoder
import kotlinx.coroutines.*
import kotlinx.serialization.encodeToString
import kotlinx.serialization.json.Json
import okhttp3.OkHttpClient
import java.io.InputStream
import java.io.OutputStream
import java.net.NetworkInterface


class MainActivity : AppCompatActivity() {
    private lateinit var _buttonUpdate: LinearLayout
    private lateinit var _text: TextView
    private lateinit var _textIPs: TextView
    private lateinit var _textProgress: TextView
    private lateinit var _updateSpinner: ImageView
    private lateinit var _imageSpinner: ImageView
    private lateinit var _layoutConnectionInfo: ConstraintLayout
    private lateinit var _videoBackground: PlayerView
    private lateinit var _viewDemo: View
    private lateinit var _player: ExoPlayer
    private lateinit var _imageQr: ImageView
    private lateinit var _textScanToConnect: TextView
    private lateinit var _systemAlertWindowPermissionLauncher: ActivityResultLauncher<Intent>
    private var _updateAvailable: Boolean? = null
    private var _updating: Boolean = false
    private var _demoClickCount = 0
    private var _lastDemoToast: Toast? = null
    private val _preferenceFileKey get() = "$packageName.PREFERENCE_FILE_KEY"

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        _systemAlertWindowPermissionLauncher = registerForActivityResult(ActivityResultContracts.StartActivityForResult()) { _ ->
            if (Settings.canDrawOverlays(this)) {
                // Permission granted, you can launch the activity from the foreground service
                Toast.makeText(this, "Alert window permission granted", Toast.LENGTH_LONG).show()
                Log.i(TAG, "Alert window permission granted")
            } else {
                // Permission denied, notify the user and request again if necessary
                Toast.makeText(this, "Permission is required to work in background", Toast.LENGTH_LONG).show()
                Log.i(TAG, "Alert window permission denied")
            }
        }

        _updateAvailable = if (savedInstanceState != null && savedInstanceState.containsKey("updateAvailable")) {
            savedInstanceState.getBoolean("updateAvailable", false)
        } else {
            null
        }

        _buttonUpdate = findViewById(R.id.button_update)
        _text = findViewById(R.id.text_dialog)
        _textIPs = findViewById(R.id.text_ips)
        _textProgress = findViewById(R.id.text_progress)
        _updateSpinner = findViewById(R.id.update_spinner)
        _imageSpinner = findViewById(R.id.image_spinner)
        _layoutConnectionInfo = findViewById(R.id.layout_connection_info)
        _videoBackground = findViewById(R.id.video_background)
        _viewDemo = findViewById(R.id.view_demo)
        _imageQr = findViewById(R.id.image_qr)
        _textScanToConnect = findViewById(R.id.text_scan_to_connect)

        startVideo()
        startAnimations()

        setText(getString(R.string.checking_for_updates))
        _buttonUpdate.visibility = View.INVISIBLE

        _buttonUpdate.setOnClickListener {
            if (_updating) {
                return@setOnClickListener
            }

            _updating = true
            update()
        }

        _viewDemo.setOnClickListener {
            _demoClickCount++
            if (_demoClickCount in 2..4) {
                val remainingClicks = 5 - _demoClickCount
                _lastDemoToast?.cancel()
                _lastDemoToast = Toast.makeText(this, "Click $remainingClicks more times to start demo", Toast.LENGTH_SHORT).apply { show() }
            } else if (_demoClickCount == 5) {
                NetworkService.instance?.onCastPlay(PlayMessage("video/mp4", "http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4"))
                _demoClickCount = 0
            }
        }

        if (BuildConfig.IS_PLAYSTORE_VERSION) {
            _text.visibility = View.INVISIBLE
            _buttonUpdate.visibility = View.INVISIBLE
            _updateSpinner.visibility = View.INVISIBLE
            (_updateSpinner.drawable as Animatable?)?.stop()
        } else {
            _text.visibility = View.VISIBLE
            _updateSpinner.visibility = View.VISIBLE
            (_updateSpinner.drawable as Animatable?)?.start()

            lifecycleScope.launch(Dispatchers.IO) {
                checkForUpdates()
            }
        }

        val ips = getIPs()
        _textIPs.text = "IPs\n${ips.joinToString("\n")}\n\nPorts\n${TcpListenerService.PORT} (TCP), ${WebSocketListenerService.PORT} (WS)"

        try {
            val barcodeEncoder = BarcodeEncoder()
            val px = TypedValue.applyDimension(TypedValue.COMPLEX_UNIT_DIP, 100.0f, resources.displayMetrics).toInt()
            val json = Json.encodeToString(FCastNetworkConfig("${Build.MANUFACTURER}-${Build.MODEL}", ips, listOf(
                FCastService(TcpListenerService.PORT, 0),
                FCastService(WebSocketListenerService.PORT, 1)
            )))
            val base64 = Base64.encodeToString(json.toByteArray(), Base64.URL_SAFE or Base64.NO_PADDING or Base64.NO_WRAP)
            val url = "fcast://r/${base64}"
            Log.i(TAG, "connection url: $url")
            val bitmap = barcodeEncoder.encodeBitmap(url, BarcodeFormat.QR_CODE, px, px)
            _imageQr.setImageBitmap(bitmap)
        } catch (e: java.lang.Exception) {
            _textScanToConnect.visibility = View.GONE
            _imageQr.visibility = View.GONE
        }

        NetworkService.activityCount++

        checkAndRequestPermissions()
        if (savedInstanceState == null) {
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
        InstallReceiver.onReceiveResult = null
        _player.release()
        NetworkService.activityCount--
    }

    override fun onSaveInstanceState(outState: Bundle) {
        super.onSaveInstanceState(outState)
        _updateAvailable?.let { outState.putBoolean("updateAvailable", it) }
    }

    private fun restartService() {
        NetworkService.instance?.stopSelf()
        startService(Intent(this, NetworkService::class.java))
    }

    private fun startVideo() {
        _player = ExoPlayer.Builder(this).build()
        _videoBackground.player = _player

        val mediaItem = MediaItem.fromUri(Uri.parse("android.resource://" + packageName + "/" + R.raw.c))
        _player.setMediaItem(mediaItem)
        _player.prepare()
        _player.repeatMode = Player.REPEAT_MODE_ALL
        _player.playWhenReady = true
    }

    private fun startAnimations() {
        //Spinner animation
        (_imageSpinner.drawable as Animatable?)?.start()
    }

    private fun checkAndRequestPermissions(): Boolean {
        val listPermissionsNeeded = arrayListOf<String>()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
            val notificationPermission = ContextCompat.checkSelfPermission(this, Manifest.permission.POST_NOTIFICATIONS)
            if (notificationPermission != PackageManager.PERMISSION_GRANTED) {
                listPermissionsNeeded.add(Manifest.permission.POST_NOTIFICATIONS)
            }
        }

        if (listPermissionsNeeded.isNotEmpty()) {
            val permissionRequestedKey = "NOTIFICATIONS_PERMISSION_REQUESTED"
            val sharedPref = this.getSharedPreferences(_preferenceFileKey, Context.MODE_PRIVATE)
            val hasRequestedPermission = sharedPref.getBoolean(permissionRequestedKey, false)
            if (!hasRequestedPermission) {
                ActivityCompat.requestPermissions(this, listPermissionsNeeded.toTypedArray(), REQUEST_ID_MULTIPLE_PERMISSIONS)
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
            val sharedPref = this.getSharedPreferences(_preferenceFileKey, Context.MODE_PRIVATE)
            val hasRequestedPermission = sharedPref.getBoolean(permissionRequestedKey, false)

            if (!Settings.canDrawOverlays(this)) {
                if (!hasRequestedPermission) {
                    AlertDialog.Builder(this)
                        .setTitle(R.string.permission_dialog_title)
                        .setMessage(R.string.permission_dialog_message)
                        .setPositiveButton(R.string.permission_dialog_positive_button) { _, _ ->
                            try {
                                val intent = Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName"))
                                _systemAlertWindowPermissionLauncher.launch(intent)
                            } catch (e: Throwable) {
                                Log.e("OverlayPermission", "Error requesting overlay permission", e)
                                Toast.makeText(this, "An error occurred: ${e.message}", Toast.LENGTH_LONG).show()
                            }
                        }
                        .setNegativeButton(R.string.permission_dialog_negative_button) { dialog, _ ->
                            dialog.dismiss()
                            Toast.makeText(this, "Permission is required to work in background", Toast.LENGTH_LONG).show()
                        }
                        .create()
                        .show()

                    with(sharedPref.edit()) {
                        putBoolean(permissionRequestedKey, true)
                        apply()
                    }
                } else {
                    Toast.makeText(this, "Optional system alert window permission missing", Toast.LENGTH_SHORT).show()
                }
            }
        } catch (e: Throwable) {
            Log.e(TAG, "Failed to request system alert window permissions")
        }
    }

    override fun onRequestPermissionsResult(requestCode: Int, permissions: Array<out String>, grantResults: IntArray) {
        super.onRequestPermissionsResult(requestCode, permissions, grantResults)

        when (requestCode) {
            REQUEST_ID_MULTIPLE_PERMISSIONS -> {
                val perms: MutableMap<String, Int> = HashMap()
                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                    perms[Manifest.permission.POST_NOTIFICATIONS] = PackageManager.PERMISSION_GRANTED
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
                            Toast.makeText(this, "Notification permission granted", Toast.LENGTH_LONG).show()
                            restartService()
                        } else {
                            Log.i(TAG, "Notification permission not granted")
                            Toast.makeText(this, "App may not fully work without notification permission", Toast.LENGTH_LONG).show()
                            restartService()
                        }
                    }
                }
            }
        }
    }

    private fun setText(text: CharSequence?) {
        if (text == null) {
            _text.visibility = View.INVISIBLE
            _text.text = ""
        } else {
            _text.visibility = View.VISIBLE
            _text.text = text
        }
    }

    private suspend fun checkForUpdates() {
        Log.i(TAG, "Checking for updates...")

        val updateAvailable = _updateAvailable
        if (updateAvailable != null) {
            setUpdateAvailable(updateAvailable)
        } else {
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
                            Toast.makeText(this@MainActivity, "Failed to retrieve version", Toast.LENGTH_LONG).show()
                        }
                    }
                } catch (e: Throwable) {
                    Log.w(TAG, "Failed to check for updates.", e)

                    withContext(Dispatchers.Main) {
                        Toast.makeText(this@MainActivity, "Failed to check for updates", Toast.LENGTH_LONG).show()
                    }
                }
            }
        }
    }

    private fun setUpdateAvailable(updateAvailable: Boolean) {
        if (updateAvailable) {
            try {
                (_updateSpinner.drawable as Animatable?)?.stop()
                _updateSpinner.visibility = View.INVISIBLE
                setText(resources.getText(R.string.there_is_an_update_available_do_you_wish_to_update))
                _buttonUpdate.visibility = View.VISIBLE
            } catch (e: Throwable) {
                Toast.makeText(this@MainActivity, "Failed to show update dialog", Toast.LENGTH_LONG).show()
                Log.w(TAG, "Error occurred in update dialog.")
            }
        } else {
            _updateSpinner.visibility = View.INVISIBLE
            _buttonUpdate.visibility = View.INVISIBLE
            setText(null)
        }

        _updateAvailable = updateAvailable
    }

    private fun downloadVersionCode(): Int? {
        val client = OkHttpClient()
        val request = okhttp3.Request.Builder()
            .method("GET", null)
            .url(VERSION_URL)
            .build()

        val response = client.newCall(request).execute()
        if (!response.isSuccessful || response.body == null) {
            return null
        }

        return response.body?.string()?.trim()?.toInt()
    }

    private fun update() {
        _updateSpinner.visibility = View.VISIBLE
        _buttonUpdate.visibility = Button.INVISIBLE
        window?.addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)

        setText(resources.getText(R.string.downloading_update))
        (_updateSpinner.drawable as Animatable?)?.start()

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
                if (response.isSuccessful && body != null) {
                    inputStream = body.byteStream()
                    val dataLength = body.contentLength()
                    install(inputStream, dataLength)
                } else {
                    throw Exception("Failed to download latest version of app.")
                }
            } catch (e: Throwable) {
                Log.w(TAG, "Exception thrown while downloading and installing latest version of app.", e)
                withContext(Dispatchers.Main) {
                    onReceiveResult("Failed to download update.")
                }
            } finally {
                inputStream?.close()
            }
        }
    }

    private suspend fun install(inputStream: InputStream, dataLength: Long) {
        var lastProgressText = ""
        var session: PackageInstaller.Session? = null

        try {
            Log.i(TAG, "Hooked InstallReceiver.onReceiveResult.")
            InstallReceiver.onReceiveResult = { message -> onReceiveResult(message) }

            val packageInstaller: PackageInstaller = packageManager.packageInstaller
            val params = PackageInstaller.SessionParams(PackageInstaller.SessionParams.MODE_FULL_INSTALL)
            val sessionId = packageInstaller.createSession(params)
            session = packageInstaller.openSession(sessionId)

            session.openWrite("package", 0, dataLength).use { sessionStream ->
                inputStream.copyToOutputStream(dataLength, sessionStream) { progress ->
                    val progressText = "${(progress * 100.0f).toInt()}%"
                    if (lastProgressText != progressText) {
                        lastProgressText = progressText

                        lifecycleScope.launch(Dispatchers.Main) {
                            _textProgress.text = progressText
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
                _textProgress.text = ""
                setText(resources.getText(R.string.installing_update))
            }
        } catch (e: Throwable) {
            Log.w(TAG, "Exception thrown while downloading and installing latest version of app.", e)
            session?.abandon()
            withContext(Dispatchers.Main) {
                onReceiveResult("Failed to download update.")
            }
        }
        finally {
            withContext(Dispatchers.Main) {
                window?.clearFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON)
            }
        }
    }

    private fun onReceiveResult(result: String?) {
        InstallReceiver.onReceiveResult = null
        Log.i(TAG, "Cleared InstallReceiver.onReceiveResult handler.")

        (_updateSpinner.drawable as Animatable?)?.stop()

        if (result.isNullOrBlank()) {
            _updateSpinner.setImageResource(R.drawable.ic_update_success)
            setText(resources.getText(R.string.success))
        } else {
            _updateSpinner.setImageResource(R.drawable.ic_update_fail)
            setText("${resources.getText(R.string.failed_to_update_with_error)}: '$result'.")
        }
    }

    private fun InputStream.copyToOutputStream(inputStreamLength: Long, outputStream: OutputStream, onProgress: (Float) -> Unit) {
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

    private fun getIPs(): List<String> {
        val ips = arrayListOf<String>()
        for (intf in NetworkInterface.getNetworkInterfaces()) {
            for (addr in intf.inetAddresses) {
                if (addr.isLoopbackAddress) {
                    continue
                }

                if (addr.address.size != 4) {
                    continue
                }

                Log.i(TAG, "Running on ${addr.hostAddress}:${TcpListenerService.PORT} (TCP)")
                Log.i(TAG, "Running on ${addr.hostAddress}:${WebSocketListenerService.PORT} (WebSocket)")
                addr.hostAddress?.let { ips.add(it) }
            }
        }
        return ips
    }

    companion object {
        const val TAG = "MainActivity"
        const val VERSION_URL = "https://releases.grayjay.app/fcast-version.txt"
        const val APK_URL = "https://releases.grayjay.app/fcast-release.apk"
        const val REQUEST_ID_MULTIPLE_PERMISSIONS = 1
        const val REQUEST_CODE = 2
    }
}