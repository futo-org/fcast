package com.futo.fcast.receiver

import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.os.Build
import android.util.Log
import androidx.core.app.NotificationCompat

class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        try {
            if (intent.action == Intent.ACTION_BOOT_COMPLETED ||
                intent.action == Intent.ACTION_PACKAGE_ADDED ||
                intent.action == Intent.ACTION_MY_PACKAGE_REPLACED) {

                if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.S) {
                    // Show a notification with an action to start the service
                    showStartServiceNotification(context);
                } else {
                    // Directly start the service for older versions
                    val serviceIntent = Intent(context, NetworkService::class.java)
                    context.startService(serviceIntent)
                }
            }
        } catch (e: Throwable) {
            Log.e("BootReceiver", "Failed to start service", e)
        }
    }

    @Suppress("DEPRECATION")
    private fun createNotificationBuilder(context: Context): NotificationCompat.Builder {
        return if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            NotificationCompat.Builder(context, CHANNEL_ID)
        } else {
            // For pre-Oreo, do not specify the channel ID
            NotificationCompat.Builder(context)
        }
    }

    private fun showStartServiceNotification(context: Context) {
        val notificationManager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

        // Create the Notification Channel for Android 8.0 and above
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val channelName = "Service Start Channel"
            val importance = NotificationManager.IMPORTANCE_DEFAULT
            val channel = NotificationChannel(CHANNEL_ID, channelName, importance)
            channel.description = "Notification Channel for Service Start"
            notificationManager.createNotificationChannel(channel)
        }

        // PendingIntent to start the TcpListenerService
        val serviceIntent = Intent(context, NetworkService::class.java)
        val pendingIntent = PendingIntent.getService(context, 0, serviceIntent, PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE)
        val startServiceAction = NotificationCompat.Action.Builder(0, "Start Service", pendingIntent).build()

        // Build the notification
        val notificationBuilder = createNotificationBuilder(context)
            .setContentTitle("Start FCast Receiver Service")
            .setContentText("Tap to start the service")
            .setSmallIcon(R.mipmap.ic_launcher)
            .addAction(startServiceAction)
            .setAutoCancel(true)

        val notification = notificationBuilder.build()

        // Notify
        notificationManager.notify(NOTIFICATION_ID, notification)
    }

    companion object {
        private const val CHANNEL_ID = "BootReceiverServiceChannel"
        private const val NOTIFICATION_ID = 1
    }
}