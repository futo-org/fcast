package org.fcast.rsreceiver.android;

import android.content.Context;
import android.net.wifi.WifiManager;
import android.util.Log;

/// The process-wide wifi multicast lease.
///
/// With the screen off the wifi chip filters multicast frames before they
/// reach the host, and mDNS goes with them: the process stays up and its
/// TCP port keeps answering (verified: 46899 open, scan silent), but no
/// query is ever delivered, so the receiver is undiscoverable until
/// somebody wakes the phone. NsdManager does not take this lock for its
/// registrations, so the app has to.
///
/// Reference counted and held by whichever of the activity or the service is
/// up. The activity's own lease spans onCreate to onDestroy and the process
/// exits with it, so the service's is a second holder rather than the only
/// one, and the counting is what keeps either one's release from dropping
/// the lock under the other.
final class MulticastLease {
    private static final String TAG = "FCastMulticast";

    private static WifiManager.MulticastLock lock = null;

    private MulticastLease() {
    }

    static synchronized void acquire(Context ctx) {
        try {
            if (lock == null) {
                WifiManager wifi = (WifiManager) ctx.getApplicationContext()
                        .getSystemService(Context.WIFI_SERVICE);
                if (wifi == null) {
                    return;
                }
                lock = wifi.createMulticastLock("FCastRsReceiver:Multicast");
                lock.setReferenceCounted(true);
            }
            lock.acquire();
        } catch (Exception e) {
            // Thrown out of acquire(), the platform's lock has already
            // counted the reference it never took: it reads held, release()
            // below skips it on the isHeld() guard, and every later acquire
            // increments a lock the service never sees. Dropped instead, so
            // the next acquire builds a fresh one and one transient failure
            // does not cost discovery for the life of the process.
            lock = null;
            Log.w(TAG, "multicast lock refused", e);
        }
    }

    static synchronized void release() {
        try {
            if (lock != null && lock.isHeld()) {
                lock.release();
            }
        } catch (Exception e) {
            Log.w(TAG, "multicast lock release refused", e);
        }
    }
}
