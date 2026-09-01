package org.fcast.rsreceiver.android;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.media.session.MediaSession;
import android.os.IBinder;

/// Keeps the receiver's process at foreground priority while the activity
/// is backgrounded, so the OS neither kills the cast nor reaps the NSD
/// registration. Also carries the media notification with a Stop action,
/// the only way to end a cast without reopening the app.
///
/// Started by MainActivity.onStop, stopped by onStart. The notification
/// content mirrors the MediaSession MainActivity owns.
public class ReceiverService extends Service {
    static final String CHANNEL_ID = "receiver";
    static final String ACTION_STOP_CAST = "org.fcast.rsreceiver.android.STOP_CAST";
    private static final int NOTIFICATION_ID = 1;

    /// The session token and current title, published by MainActivity for
    /// the notification. Static because the service and activity live in
    /// one process and the service can (re)build its notification at any
    /// point of its lifecycle.
    static volatile MediaSession.Token sessionToken = null;
    static volatile String castTitle = null;
    static volatile boolean castActive = false;

    private static volatile ReceiverService running = null;

    static void refreshIfRunning() {
        ReceiverService service = running;
        if (service != null) {
            NotificationManager nm = service.getSystemService(NotificationManager.class);
            nm.notify(NOTIFICATION_ID, service.buildNotification());
        }
    }

    static void ensureChannel(Context ctx) {
        NotificationManager nm = ctx.getSystemService(NotificationManager.class);
        NotificationChannel channel = new NotificationChannel(
                CHANNEL_ID,
                ctx.getString(R.string.app_name),
                NotificationManager.IMPORTANCE_LOW);
        channel.setShowBadge(false);
        nm.createNotificationChannel(channel);
    }

    private Notification buildNotification() {
        Intent open = new Intent(this, MainActivity.class);
        open.setFlags(Intent.FLAG_ACTIVITY_SINGLE_TOP);
        PendingIntent openPi = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_IMMUTABLE);

        Notification.Builder b = new Notification.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.mipmap.ic_launcher)
                .setContentIntent(openPi)
                .setOngoing(true);

        String title = castTitle;
        if (castActive) {
            b.setContentTitle(title == null || title.isEmpty()
                    ? getString(R.string.notification_casting)
                    : title);
            b.setContentText(getString(R.string.notification_casting));
            Intent stop = new Intent(this, ReceiverService.class);
            stop.setAction(ACTION_STOP_CAST);
            PendingIntent stopPi = PendingIntent.getService(
                    this, 1, stop, PendingIntent.FLAG_IMMUTABLE);
            b.addAction(new Notification.Action.Builder(
                    null, getString(R.string.notification_stop), stopPi).build());
            Notification.MediaStyle style = new Notification.MediaStyle();
            MediaSession.Token token = sessionToken;
            if (token != null) {
                style.setMediaSession(token);
            }
            b.setStyle(style);
        } else {
            b.setContentTitle(getString(R.string.notification_ready));
        }
        return b.build();
    }

    @Override
    public int onStartCommand(Intent intent, int flags, int startId) {
        if (intent != null && ACTION_STOP_CAST.equals(intent.getAction())) {
            MainActivity.nativeMediaCommand(0);
            return START_NOT_STICKY;
        }
        ensureChannel(this);
        if (android.os.Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, buildNotification(),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK);
        } else {
            startForeground(NOTIFICATION_ID, buildNotification());
        }
        running = this;
        return START_NOT_STICKY;
    }

    @Override
    public void onDestroy() {
        running = null;
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
