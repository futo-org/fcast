package org.fcast.android.sender;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.Service;
import android.content.Intent;
import android.os.IBinder;
import android.util.Log;

import androidx.annotation.Nullable;
import androidx.core.app.NotificationCompat;

public class ScreenCaptureService extends Service {
    public ScreenCaptureService() {
    }

    @Override
    public void onCreate() {
        super.onCreate();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && intent.getAction().equals(MainActivity.ACTION_RESULT)) {
            startForegroundService();
        }
        return START_STICKY;
    }

    private void startForegroundService() {
        Log.d("SCREEN_CAPTURE", "starting foreground service");

        String channelId = "ScreenCaptureChannel";

        // For API >=26, we're at least that, always
        NotificationChannel channel = new NotificationChannel(channelId, "Screen Capture Service", NotificationManager.IMPORTANCE_LOW);
        NotificationManager manager = getSystemService(NotificationManager.class);
        manager.createNotificationChannel(channel);

        Notification notification = new NotificationCompat.Builder(this, channelId).setContentTitle("Screen Capture").setContentText("Capturing screen...").build();

        startForeground(1, notification);
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
