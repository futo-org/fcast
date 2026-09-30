package org.fcast.rsreceiver.android;

import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.pm.PackageInstaller;

/** PackageInstaller's verdict on a self-update session (Updater). */
public class UpdateStatusReceiver extends BroadcastReceiver {
    @Override
    @SuppressWarnings("deprecation")
    public void onReceive(Context ctx, Intent intent) {
        int status = intent.getIntExtra(
                PackageInstaller.EXTRA_STATUS, PackageInstaller.STATUS_FAILURE);
        switch (status) {
            case PackageInstaller.STATUS_PENDING_USER_ACTION: {
                Intent confirm = android.os.Build.VERSION.SDK_INT >= 33
                        ? intent.getParcelableExtra(Intent.EXTRA_INTENT, Intent.class)
                        : intent.getParcelableExtra(Intent.EXTRA_INTENT);
                if (confirm == null) {
                    Updater.installFailed(ctx, "the installer asked for a confirmation it did not provide");
                } else {
                    Updater.confirm(ctx, confirm.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK));
                }
                return;
            }
            case PackageInstaller.STATUS_SUCCESS:
                // the new version's MY_PACKAGE_REPLACED takes it from here
                return;
            case PackageInstaller.STATUS_FAILURE_ABORTED:
                Updater.installFailed(ctx, "cancelled");
                return;
            case PackageInstaller.STATUS_FAILURE_BLOCKED:
                Updater.installFailed(ctx, "blocked by the system");
                return;
            case PackageInstaller.STATUS_FAILURE_CONFLICT:
                Updater.installFailed(ctx, "conflicts with the installed app");
                return;
            case PackageInstaller.STATUS_FAILURE_INCOMPATIBLE:
                Updater.installFailed(ctx, "not compatible with this device");
                return;
            case PackageInstaller.STATUS_FAILURE_INVALID:
                Updater.installFailed(ctx, "the package is invalid");
                return;
            case PackageInstaller.STATUS_FAILURE_STORAGE:
                Updater.installFailed(ctx, "not enough storage");
                return;
            default: {
                String msg = intent.getStringExtra(PackageInstaller.EXTRA_STATUS_MESSAGE);
                Updater.installFailed(ctx, msg != null ? msg : "failed with status " + status);
            }
        }
    }
}
