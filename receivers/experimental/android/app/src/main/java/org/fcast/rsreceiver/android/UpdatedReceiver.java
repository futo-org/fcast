package org.fcast.rsreceiver.android;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;

/** Runs in the new version once an update replaced this app. */
public class UpdatedReceiver extends BroadcastReceiver {
    @Override
    public void onReceive(Context ctx, Intent intent) {
        if (Intent.ACTION_MY_PACKAGE_REPLACED.equals(intent.getAction())) {
            Updater.relaunchAfterUpdate(ctx);
        }
    }
}
