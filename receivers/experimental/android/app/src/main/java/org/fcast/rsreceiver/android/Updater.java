package org.fcast.rsreceiver.android;

import android.app.Activity;
import android.app.Notification;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.content.SharedPreferences;
import android.content.pm.ApplicationInfo;
import android.content.pm.PackageInfo;
import android.content.pm.PackageInstaller;
import android.content.pm.PackageManager;
import android.content.pm.Signature;
import android.content.pm.SigningInfo;
import android.util.Log;
import java.io.File;
import java.io.FileInputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.security.MessageDigest;
import java.security.NoSuchAlgorithmException;
import java.util.Arrays;

/**
 * Self-update for the sideloaded build. Native code checks the channel and
 * downloads the apk (android_updater.rs), this side verifies it and hands it
 * to PackageInstaller.
 */
final class Updater {
    private static final String TAG = "Updater";

    // Mirrored in android_updater.rs.
    static final int MODE_OFF = 0;
    static final int MODE_RELEASE = 1;
    static final int MODE_DEBUG = 2;

    // The certificate the published apks are signed with.
    private static final String RELEASE_CERT_SHA256 =
            "ef81eda98f617f7f1707d80509f28dae4e50f56e41e077eb3024d9c9072d074e";

    private static final String PREFS = "updater";
    private static final String KEY_RELAUNCH_AT = "relaunch_at";
    // An older marker belongs to an install that never completed.
    private static final long RELAUNCH_WINDOW_MS = 30 * 60 * 1000;
    private static final int UPDATED_NOTIFICATION_ID = 2;

    // False in a process that never loaded the library, like one started
    // only to deliver an install status.
    static volatile boolean nativeLoaded = false;

    // The installer's confirmation, held while the activity is stopped:
    // background activity starts are blocked from 10 on.
    private static Intent pendingConfirm = null;
    private static boolean activityStarted = false;

    private Updater() {}

    static native void nativeInstallFailed(String message);

    /**
     * Off for the Play build, which has no install permission, and for a
     * build that is neither release-signed nor debuggable: no published
     * update could install over it. Debug builds only update from a channel
     * set by hand (android_updater.rs).
     */
    static int mode(Context ctx) {
        PackageInfo info = selfInfo(ctx,
                PackageManager.GET_PERMISSIONS | PackageManager.GET_SIGNING_CERTIFICATES);
        if (info == null || info.requestedPermissions == null
                || !Arrays.asList(info.requestedPermissions)
                        .contains(android.Manifest.permission.REQUEST_INSTALL_PACKAGES)) {
            return MODE_OFF;
        }
        if (info.signingInfo != null && signedBy(info.signingInfo, RELEASE_CERT_SHA256)) {
            return MODE_RELEASE;
        }
        if ((ctx.getApplicationInfo().flags & ApplicationInfo.FLAG_DEBUGGABLE) != 0) {
            return MODE_DEBUG;
        }
        return MODE_OFF;
    }

    static long installedVersionCode(Context ctx) {
        PackageInfo info = selfInfo(ctx, 0);
        return info == null ? -1 : info.getLongVersionCode();
    }

    private static File downloadFile(Context ctx) {
        return new File(ctx.getCacheDir(), "update.apk");
    }

    /** The download target, cleared of an earlier attempt. */
    static String prepareDownload(Context ctx) {
        File f = downloadFile(ctx);
        //noinspection ResultOfMethodCallIgnored
        f.delete();
        return f.getAbsolutePath();
    }

    /** Asynchronous, failures come back through nativeInstallFailed. */
    static void install(Context ctx, String path, long versionCode) {
        Context app = ctx.getApplicationContext();
        new Thread(() -> {
            String err = installBlocking(app, new File(path), versionCode);
            if (err != null) {
                installFailed(app, err);
            }
        }, "fcast-update").start();
    }

    private static String installBlocking(Context ctx, File apk, long versionCode) {
        PackageManager pm = ctx.getPackageManager();
        try {
            PackageInfo archive = pm.getPackageArchiveInfo(
                    apk.getPath(), PackageManager.GET_SIGNING_CERTIFICATES);
            if (archive == null) {
                return "the download is not a valid package";
            }
            if (!ctx.getPackageName().equals(archive.packageName)) {
                return "the download is for another app";
            }
            if (archive.getLongVersionCode() != versionCode) {
                return "the download is not the offered version";
            }
            // Legible and before the system dialog, the installer enforces
            // the key regardless. Some releases parse no signers from an
            // archive, then this is skipped.
            PackageInfo self = selfInfo(ctx, PackageManager.GET_SIGNING_CERTIFICATES);
            if (archive.signingInfo != null && self != null && self.signingInfo != null
                    && !sameSigner(archive.signingInfo, self.signingInfo)) {
                return "the download is signed with a different key";
            }

            PackageInstaller installer = pm.getPackageInstaller();
            PackageInstaller.SessionParams params = new PackageInstaller.SessionParams(
                    PackageInstaller.SessionParams.MODE_FULL_INSTALL);
            params.setAppPackageName(ctx.getPackageName());
            params.setSize(apk.length());
            PackageInstaller.Session session = installer.openSession(installer.createSession(params));
            boolean committed = false;
            try {
                try (InputStream in = new FileInputStream(apk);
                     OutputStream out = session.openWrite("base.apk", 0, apk.length())) {
                    byte[] buf = new byte[1 << 16];
                    int n;
                    while ((n = in.read(buf)) > 0) {
                        out.write(buf, 0, n);
                    }
                    session.fsync(out);
                }
                prefs(ctx).edit().putLong(KEY_RELAUNCH_AT, System.currentTimeMillis()).commit();
                PendingIntent status = PendingIntent.getBroadcast(ctx, 0,
                        new Intent(ctx, UpdateStatusReceiver.class),
                        PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_MUTABLE);
                session.commit(status.getIntentSender());
                committed = true;
            } finally {
                if (!committed) {
                    session.abandon();
                }
                session.close();
            }
            return null;
        } catch (IOException | RuntimeException e) {
            Log.w(TAG, "install failed", e);
            return e.getMessage() != null ? e.getMessage() : e.toString();
        } finally {
            // the session holds its own copy once written
            //noinspection ResultOfMethodCallIgnored
            apk.delete();
        }
    }

    /** The installer wants the user to confirm, from UpdateStatusReceiver. */
    static synchronized void confirm(Context ctx, Intent confirm) {
        if (activityStarted) {
            ctx.startActivity(confirm);
        } else {
            pendingConfirm = confirm;
        }
    }

    static void installFailed(Context ctx, String message) {
        Log.w(TAG, "update not installed: " + message);
        prefs(ctx).edit().remove(KEY_RELAUNCH_AT).apply();
        synchronized (Updater.class) {
            pendingConfirm = null;
        }
        if (nativeLoaded) {
            nativeInstallFailed(message);
        }
    }

    static void onActivityCreated(Context ctx) {
        // a finished install leaves its notification and never cleans up
        ctx.getSystemService(NotificationManager.class).cancel(UPDATED_NOTIFICATION_ID);
        prepareDownload(ctx);
    }

    static synchronized void onActivityStarted(Activity activity) {
        activityStarted = true;
        Intent confirm = pendingConfirm;
        pendingConfirm = null;
        if (confirm != null) {
            activity.startActivity(confirm);
        }
    }

    static synchronized void onActivityStopped() {
        activityStarted = false;
    }

    /**
     * The in-app update just replaced this app, put the receiver back on
     * screen. Only after an in-app update, a Play or adb update of a
     * backgrounded app should not pop it up.
     */
    static void relaunchAfterUpdate(Context ctx) {
        SharedPreferences prefs = prefs(ctx);
        long at = prefs.getLong(KEY_RELAUNCH_AT, 0);
        prefs.edit().remove(KEY_RELAUNCH_AT).commit();
        long age = System.currentTimeMillis() - at;
        if (at == 0 || age < 0 || age > RELAUNCH_WINDOW_MS) {
            return;
        }
        Intent launch = ctx.getPackageManager().getLaunchIntentForPackage(ctx.getPackageName());
        if (launch == null) {
            return;
        }
        launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        PendingIntent open = PendingIntent.getActivity(ctx, 3, launch,
                PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE);

        // The notification is the fallback for a refused background start
        // (no overlay grant). The activity cancels it when it does come up.
        ReceiverService.ensureChannel(ctx);
        Notification n = new Notification.Builder(ctx, ReceiverService.CHANNEL_ID)
                .setSmallIcon(R.drawable.ic_stat_cast)
                .setContentTitle(ctx.getString(R.string.app_name))
                .setContentText(ctx.getString(R.string.notification_updated))
                .setContentIntent(open)
                .setAutoCancel(true)
                .build();
        ctx.getSystemService(NotificationManager.class).notify(UPDATED_NOTIFICATION_ID, n);

        // the same route as MainActivity.bringToFront
        if (android.os.Build.VERSION.SDK_INT < 29
                || android.provider.Settings.canDrawOverlays(ctx)) {
            try {
                open.send();
            } catch (PendingIntent.CanceledException e) {
                Log.w(TAG, "relaunch after update refused", e);
            }
        }
    }

    private static SharedPreferences prefs(Context ctx) {
        return ctx.getSharedPreferences(PREFS, Context.MODE_PRIVATE);
    }

    private static PackageInfo selfInfo(Context ctx, int flags) {
        try {
            return ctx.getPackageManager().getPackageInfo(ctx.getPackageName(), flags);
        } catch (PackageManager.NameNotFoundException e) {
            return null;
        }
    }

    // The current signers, or the rotation history of a single signer.
    private static Signature[] signers(SigningInfo info) {
        Signature[] s = info.hasMultipleSigners()
                ? info.getApkContentsSigners()
                : info.getSigningCertificateHistory();
        return s == null ? new Signature[0] : s;
    }

    private static boolean signedBy(SigningInfo info, String sha256Hex) {
        for (Signature s : signers(info)) {
            if (sha256Hex.equals(sha256(s.toByteArray()))) {
                return true;
            }
        }
        return false;
    }

    private static boolean sameSigner(SigningInfo a, SigningInfo b) {
        for (Signature s : signers(a)) {
            for (Signature t : signers(b)) {
                if (s.equals(t)) {
                    return true;
                }
            }
        }
        return false;
    }

    private static String sha256(byte[] data) {
        try {
            byte[] d = MessageDigest.getInstance("SHA-256").digest(data);
            StringBuilder hex = new StringBuilder(d.length * 2);
            for (byte b : d) {
                hex.append(String.format("%02x", b & 0xff));
            }
            return hex.toString();
        } catch (NoSuchAlgorithmException e) {
            return "";
        }
    }
}
