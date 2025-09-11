package com.futo.fcast.receiver

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.pm.PackageInstaller
import android.os.Build
import android.util.Log

class InstallReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val status = intent.getIntExtra(PackageInstaller.EXTRA_STATUS, -1)
        Log.i(TAG, "Received status $status.")

        when (status) {
            PackageInstaller.STATUS_PENDING_USER_ACTION -> {
                val activityIntent: Intent? =
                    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
                        intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent::class.java)
                    } else {
                        @Suppress("DEPRECATION")
                        intent.getParcelableExtra(Intent.EXTRA_INTENT)
                    }

                if (activityIntent == null) {
                    Log.w(TAG, "Received STATUS_PENDING_USER_ACTION and activity intent is null.")
                    return
                }
                context.startActivity(activityIntent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            }

            PackageInstaller.STATUS_SUCCESS -> onReceiveResult?.invoke(null)
            PackageInstaller.STATUS_FAILURE -> onReceiveResult?.invoke(context.getString(R.string.general_failure))
            PackageInstaller.STATUS_FAILURE_ABORTED -> onReceiveResult?.invoke(context.getString(R.string.aborted))
            PackageInstaller.STATUS_FAILURE_BLOCKED -> onReceiveResult?.invoke(context.getString(R.string.blocked))
            PackageInstaller.STATUS_FAILURE_CONFLICT -> onReceiveResult?.invoke(context.getString(R.string.conflict))
            PackageInstaller.STATUS_FAILURE_INCOMPATIBLE -> onReceiveResult?.invoke(
                context.getString(
                    R.string.incompatible
                )
            )

            PackageInstaller.STATUS_FAILURE_INVALID -> onReceiveResult?.invoke(context.getString(R.string.invalid))
            PackageInstaller.STATUS_FAILURE_STORAGE -> onReceiveResult?.invoke(context.getString(R.string.not_enough_storage))
            else -> {
                val msg = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE)
                onReceiveResult?.invoke(msg)
            }
        }
    }

    companion object {
        const val TAG = "InstallReceiver"
        var onReceiveResult: ((String?) -> Unit)? = null
    }
}