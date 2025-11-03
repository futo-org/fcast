package org.fcast.android.sender;

import static org.fcast.android.sender.MainActivity.ACTION_MEDIA_PROJECTION_STARTED;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.os.Build;
import android.os.IBinder;
import android.util.Log;

import androidx.annotation.Nullable;
import androidx.localbroadcastmanager.content.LocalBroadcastManager;

public class ScreenCaptureService extends Service {
    private static final String TAG = "ScreenCaptureService";

    private Notification notification;

    public ScreenCaptureService() {
    }

    @Override
    public void onCreate() {
        super.onCreate();

        Log.d(TAG, "onCreate");

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            String NOTIF_CHANNEL_ID = "org.fcast.android.sender.ScreenCaptureService";
            NotificationChannel channel = new NotificationChannel(NOTIF_CHANNEL_ID, "ScreenCaptureService", NotificationManager.IMPORTANCE_NONE);
            channel.setLockscreenVisibility(Notification.VISIBILITY_PRIVATE);
            NotificationManager manager = (NotificationManager) getSystemService(Context.NOTIFICATION_SERVICE);
            if (manager != null) {
                manager.createNotificationChannel(channel);
                notification = new Notification.Builder(this, channel.getId()).build();
            }
        }
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        Log.d(TAG, "onStartCommand intent=" + intent);

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            int resultCode = intent.getIntExtra("resultCode", -1);
            Intent data = intent.getParcelableExtra("data");

            Intent broadcastIntent = new Intent(this, MainActivity.CaptureBroadcastReceiver.class);
            broadcastIntent.setAction(ACTION_MEDIA_PROJECTION_STARTED);
            broadcastIntent.putExtra("resultCode", resultCode);
            broadcastIntent.putExtra("data", data);

            startForeground(1, notification);

            Log.d(TAG, "Started foreground");

            LocalBroadcastManager.getInstance(this).sendBroadcast(broadcastIntent);
        }

        return START_STICKY;
    }

    public void stopCapture() {
        stopForeground(true);

        stopSelf();
    }

    @Override
    public void onDestroy() {
        stopCapture();
        super.onDestroy();
    }

    @Nullable
    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
