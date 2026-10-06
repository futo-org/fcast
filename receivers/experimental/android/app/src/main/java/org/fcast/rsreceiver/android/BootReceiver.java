package org.fcast.rsreceiver.android;

import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.util.Log;

/// Starts the receiver after a boot or an update, while start on boot is on
/// (the component is disabled otherwise), and after the upgrade from the
/// Kotlin receiver, see the manifest. Boot is exempt from the background
/// service start ban, and specialUse may start from it on 15+.
public class BootReceiver extends BroadcastReceiver {
    // 1 service, 2 updater, 3 cast waiting
    private static final int TAP_TO_START_NOTIFICATION_ID = 4;

    @Override
    public void onReceive(Context ctx, Intent intent) {
        String action = intent.getAction();
        if (!Intent.ACTION_BOOT_COMPLETED.equals(action)
                && !Intent.ACTION_MY_PACKAGE_REPLACED.equals(action)) {
            return;
        }
        Intent service = new Intent(ctx, ReceiverService.class);
        try {
            ctx.startForegroundService(service);
        } catch (Exception e) {
            // OEM builds that refuse it anyway: let the user start it
            Log.w("FCastBootReceiver", "service start refused, posting tap-to-start", e);
            ReceiverService.ensureChannel(ctx);
            PendingIntent start = PendingIntent.getForegroundService(ctx, 5, service,
                    PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);
            ctx.getSystemService(NotificationManager.class).notify(TAP_TO_START_NOTIFICATION_ID,
                    new Notification.Builder(ctx, ReceiverService.CHANNEL_ID)
                            .setSmallIcon(R.drawable.ic_stat_cast)
                            .setContentTitle(ctx.getString(R.string.app_name))
                            .setContentText(ctx.getString(R.string.notification_tap_to_start))
                            .setContentIntent(start)
                            .setAutoCancel(true)
                            .build());
        }
    }
}
