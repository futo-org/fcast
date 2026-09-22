package org.fcast.rsreceiver.android;

import android.app.Notification;
import android.app.NotificationChannel;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.app.Service;
import android.content.Context;
import android.content.Intent;
import android.content.pm.ServiceInfo;
import android.graphics.drawable.Icon;
import android.media.session.MediaSession;
import android.os.Handler;
import android.os.IBinder;
import android.os.Looper;

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
    // Rebuilds are posted: callers include rust worker threads, and a post
    // serializes against onDestroy on the same looper, so a refresh can
    // never notify past a dead service and orphan the ongoing notification.
    private static final Handler mainHandler = new Handler(Looper.getMainLooper());

    static void refreshIfRunning() {
        mainHandler.post(() -> {
            ReceiverService service = running;
            if (service == null) {
                return;
            }
            if (service.foregroundType == service.wantedType()) {
                // plain notification refresh, no ActivityManager round trip
                NotificationManager nm = service.getSystemService(NotificationManager.class);
                nm.notify(NOTIFICATION_ID, service.buildNotification());
            } else {
                service.goForeground();
            }
        });
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
        open.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        PendingIntent openPi = PendingIntent.getActivity(
                this, 0, open, PendingIntent.FLAG_IMMUTABLE);

        Notification.Builder b = new Notification.Builder(this, CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_stat_cast)
                .setContentIntent(openPi)
                .setCategory(Notification.CATEGORY_TRANSPORT)
                .setVisibility(Notification.VISIBILITY_PUBLIC)
                .setOngoing(true);
        if (android.os.Build.VERSION.SDK_INT >= 31) {
            // otherwise the system defers FGS notifications ~10s and the
            // Stop control is invisible right when the user backgrounds
            b.setForegroundServiceBehavior(Notification.FOREGROUND_SERVICE_IMMEDIATE);
        }

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
                    Icon.createWithResource(this, R.drawable.ic_stat_stop),
                    getString(R.string.notification_stop), stopPi).build());
            Notification.MediaStyle style = new Notification.MediaStyle()
                    // the collapsed row is where most users look; without
                    // this the Stop action only exists expanded
                    .setShowActionsInCompactView(0);
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
            if (running == null) {
                // started only to carry the action; do not linger
                stopSelf(startId);
            }
            return START_NOT_STICKY;
        }
        ensureChannel(this);
        goForeground();
        running = this;
        // Backgrounded, the service is the process: it carries the
        // multicast lease so an idle receiver stays discoverable (see
        // MulticastLease). Once per instance, onDestroy drops the one.
        if (!leased) {
            leased = true;
            MulticastLease.acquire(this);
        }
        return START_NOT_STICKY;
    }

    private int foregroundType = -1;
    private boolean leased = false;

    private int wantedType() {
        if (android.os.Build.VERSION.SDK_INT >= 34) {
            return castActive
                    ? ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
                    : ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE;
        }
        return ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK;
    }

    private void goForegroundInner() {
        if (android.os.Build.VERSION.SDK_INT >= 34) {
            // the honest split: specialUse exists from 34, which is also
            // where the per-type policy enforcement lives
            startForeground(NOTIFICATION_ID, buildNotification(), castActive
                    ? ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK
                    : ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE);
        } else if (android.os.Build.VERSION.SDK_INT >= 29) {
            startForeground(NOTIFICATION_ID, buildNotification(),
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_MEDIA_PLAYBACK);
        } else {
            startForeground(NOTIFICATION_ID, buildNotification());
        }
        foregroundType = wantedType();
    }

    private void goForeground() {
        // startForeground throws for a handful of policy reasons and this
        // runs on metadata pushes mid-cast; a throw must not kill the cast
        try {
            goForegroundInner();
        } catch (Exception e) {
            android.util.Log.w("FCastReceiverService", "startForeground refused", e);
        }
    }

    @Override
    public void onTaskRemoved(Intent rootIntent) {
        // Recents swipe: the process is going down with the task; take the
        // notification along instead of leaving an orphan.
        stopSelf();
        super.onTaskRemoved(rootIntent);
    }

    @Override
    public void onDestroy() {
        running = null;
        if (leased) {
            leased = false;
            MulticastLease.release();
        }
        super.onDestroy();
    }

    @Override
    public IBinder onBind(Intent intent) {
        return null;
    }
}
