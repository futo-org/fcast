package com.futo.fcast.receiver

import android.Manifest
import android.app.AlertDialog
import android.app.PendingIntent
import android.content.Intent
import android.content.pm.PackageInstaller
import android.content.pm.PackageManager
import android.graphics.drawable.Animatable
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.provider.Settings
import android.util.Log
import android.view.View
import android.view.WindowManager
import android.widget.*
import androidx.appcompat.app.AppCompatActivity
import androidx.core.app.ActivityCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.*
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
    private lateinit var _layoutUpdate: LinearLayout;
    private var _updating: Boolean = false

    private val _scope: CoroutineScope = CoroutineScope(Dispatchers.Main)

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_main)

        _buttonUpdate = findViewById(R.id.button_update)
        _text = findViewById(R.id.text_dialog)
        _textIPs = findViewById(R.id.text_ips)
        _textProgress = findViewById(R.id.text_progress)
        _updateSpinner = findViewById(R.id.update_spinner)
        _layoutUpdate = findViewById(R.id.layout_update)

        _text.text = getString(R.string.checking_for_updates)
        _buttonUpdate.visibility = View.INVISIBLE

        _buttonUpdate.setOnClickListener {
            if (_updating) {
                return@setOnClickListener
            }

            _updating = true
            update()
        }

        if (BuildConfig.IS_PLAYSTORE_VERSION) {
            _layoutUpdate.visibility = View.GONE
            _updateSpinner.visibility = View.GONE
            (_updateSpinner.drawable as Animatable?)?.stop()
        } else {
            _layoutUpdate.visibility = View.VISIBLE
            _updateSpinner.visibility = View.VISIBLE
            (_updateSpinner.drawable as Animatable?)?.start()

            _scope.launch(Dispatchers.IO) {
                checkForUpdates()
            }
        }

        _textIPs.text = "IPs\n" + getIPs().joinToString("\n") + "\n\nPort\n46899"
        TcpListenerService.activityCount++

        if (checkAndRequestPermissions()) {
            Log.i(TAG, "Notification permission already granted")
            restartService()
        } else {
            restartService()
        }

        requestSystemAlertWindowPermission()
    }

    override fun onDestroy() {
        super.onDestroy()
        InstallReceiver.onReceiveResult = null
        _scope.cancel()
        TcpListenerService.activityCount--
    }

    private fun restartService() {
        val i = TcpListenerService.instance
        if (i != null) {
            i.stopSelf()
        }

        startService(Intent(this, TcpListenerService::class.java))
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
            ActivityCompat.requestPermissions(this, listPermissionsNeeded.toTypedArray(), REQUEST_ID_MULTIPLE_PERMISSIONS)
            return false
        }

        return true
    }

    private fun requestSystemAlertWindowPermission() {
        if (!Settings.canDrawOverlays(this)) {
            AlertDialog.Builder(this)
                .setTitle(R.string.permission_dialog_title)
                .setMessage(R.string.permission_dialog_message)
                .setPositiveButton(R.string.permission_dialog_positive_button) { _, _ ->
                    val intent = Intent(Settings.ACTION_MANAGE_OVERLAY_PERMISSION, Uri.parse("package:$packageName"))
                    startActivityForResult(intent, REQUEST_CODE)
                }
                .setNegativeButton(R.string.permission_dialog_negative_button) { dialog, _ ->
                    dialog.dismiss()
                    Toast.makeText(this, "Permission is required to work in background", Toast.LENGTH_LONG).show()
                }
                .create()
                .show()
        }
    }

    override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)

        if (requestCode == REQUEST_CODE) {
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
        super.onActivityResult(requestCode, resultCode, data)
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

    private suspend fun checkForUpdates() {
        Log.i(TAG, "Checking for updates...");

        withContext(Dispatchers.IO) {
            try {
                val latestVersion = downloadVersionCode()

                if (latestVersion != null) {
                    val currentVersion = BuildConfig.VERSION_CODE;
                    Log.i(TAG, "Current version $currentVersion latest version $latestVersion.");

                    if (latestVersion > currentVersion) {
                        withContext(Dispatchers.Main) {
                            try {
                                (_updateSpinner.drawable as Animatable?)?.stop()
                                _updateSpinner.visibility = View.INVISIBLE
                                _text.text = resources.getText(R.string.there_is_an_update_available_do_you_wish_to_update)
                                _buttonUpdate.visibility = View.VISIBLE
                            } catch (e: Throwable) {
                                Toast.makeText(this@MainActivity, "Failed to show update dialog", Toast.LENGTH_LONG).show();
                                Log.w(TAG, "Error occurred in update dialog.");
                            }
                        }
                    } else {
                        withContext(Dispatchers.Main) {
                            _updateSpinner.visibility = View.INVISIBLE
                            _text.text = getString(R.string.no_updates_available)
                            Toast.makeText(this@MainActivity, "Already on latest version", Toast.LENGTH_LONG).show();
                        }
                    }
                } else {
                    Log.w(TAG, "Failed to retrieve version from version URL.");

                    withContext(Dispatchers.Main) {
                        Toast.makeText(this@MainActivity, "Failed to retrieve version", Toast.LENGTH_LONG).show();
                    }
                }
            } catch (e: Throwable) {
                Log.w(TAG, "Failed to check for updates.", e);

                withContext(Dispatchers.Main) {
                    Toast.makeText(this@MainActivity, "Failed to check for updates", Toast.LENGTH_LONG).show();
                }
            }
        }
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

        _text.text = resources.getText(R.string.downloading_update)
        (_updateSpinner.drawable as Animatable?)?.start()

        _scope.launch(Dispatchers.IO) {
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

                        //TODO: Use proper scope
                        GlobalScope.launch(Dispatchers.Main) {
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
                _text.text = resources.getText(R.string.installing_update)
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

        if (result == null || result.isBlank()) {
            _updateSpinner.setImageResource(R.drawable.ic_update_success)
            _text.text = resources.getText(R.string.success)
        } else {
            _updateSpinner.setImageResource(R.drawable.ic_update_fail)
            _text.text = "${resources.getText(R.string.failed_to_update_with_error)}: '$result'."
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

                Log.i(TcpListenerService.TAG, "Running on ${addr.hostAddress}:${TcpListenerService.PORT}")
                addr.hostAddress?.let { ips.add(it) }
            }
        }
        return ips;
    }

    companion object {
        const val TAG = "MainActivity"
        const val VERSION_URL = "https://releases.grayjay.app/fcast-version.txt"
        const val APK_URL = "https://releases.grayjay.app/fcast-release.apk"
        const val REQUEST_ID_MULTIPLE_PERMISSIONS = 1
        const val REQUEST_CODE = 2
    }
}