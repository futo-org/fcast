package org.fcast.rsreceiver.android;

import android.annotation.SuppressLint;
import android.app.NotificationManager;
import android.app.PendingIntent;
import android.content.Context;
import android.content.Intent;
import android.media.AudioManager;
import android.media.session.MediaSession;
import android.net.ConnectivityManager;
import android.net.LinkProperties;
import android.net.Network;
import android.net.NetworkCapabilities;
import android.net.NetworkRequest;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.net.wifi.WifiManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.os.Looper;
import android.os.PowerManager;
import android.util.Log;
import androidx.annotation.NonNull;
import java.lang.ref.WeakReference;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.Enumeration;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.regex.Pattern;

/// The receiver's process-level half: discovery, the network sweep, the
/// locks, the media session, audio focus and bringing a cast forward. Lives
/// as long as the process, whichever of the activity and ReceiverService
/// started it, so the receiver outlives its UI (ANDROID-BOOT-START-PLAN.md).
/// Main thread unless a method says otherwise; native calls the static
/// methods below from any thread.
public final class ReceiverCore {
    private static final String TAG = "FCastReceiverCore";

    static {
        // One self-contained library: GStreamer is statically linked inside
        // and initialized by the native side, no java glue involved.
        System.loadLibrary("fcastreceiver");
        Updater.nativeLoaded = true;
    }

    private ReceiverCore() { }

    private static final Handler handler = new Handler(Looper.getMainLooper());
    private static Context app = null;
    private static boolean started = false;

    // The activity showing the UI, if any. Window-only work goes through it.
    private static WeakReference<MainActivity> activity = new WeakReference<>(null);
    private static boolean uiVisible = false;

    // Interface sweeps walk /proc/net and issue an ioctl per interface,
    // tens of ms on some devices: off the main looper.
    private static HandlerThread netThread = null;
    private static Handler netHandler = null;

    private static NsdManager nsdManager = null;
    private static WifiManager wifiManager = null;
    private static WifiManager.WifiLock wifiLock = null;
    private static int wifiLockMode = -1;
    private static PowerManager.WakeLock cpuWakeLock = null;
    private static ConnectivityManager connectivityManager = null;

    /// The name the registration always uses, from the settings, null while
    /// FCast is disabled. Never reassigned: adopting a collision-renamed value
    /// as the new base compounds " (2)" suffixes on every re-registration cycle.
    private static String fcastServiceName;
    // The listener instance IS the registration handle: one fresh instance per
    // register call, never reused, so re-registration cycles cannot trip
    // NsdManager's listener-in-use checks.
    private static NsdListener fcastReg = null;
    /// Cancels stale registerFCastWhenReady poll chains: each
    /// registerServices() bumps it and in-flight lambdas holding an older
    /// value stop, so two overlapping chains cannot both register.
    private static int fcastPollGen = 0;
    /// Registration backoff and its single-retry-pending flag: only one retry
    /// is ever queued, or back-to-back failures would fan out exponentially.
    private static int fcastRetries = 0;
    private static boolean fcastRetryPending = false;

    // Re-sweep even without a callback: tethering/hotspot interfaces never
    // surface as Networks, so their addresses are only found by polling.
    private static final long SWEEP_INTERVAL_MS = 30_000;
    private static final long NETWORK_SETTLE_MS = 500;

    private static boolean castActive = false;
    private static boolean castVisual = false;
    /// The cast came in while the UI was in the background, so its end puts
    /// the receiver back there instead of on the idle screen. Main thread.
    private static boolean castFromBackground = false;
    /// A cast is coming forward and may turn the screen on for it, until the
    /// window has focus or the cast goes idle. Main thread.
    private static boolean castWake = false;
    private static boolean castPlaying = false;
    /// Paused by the player, and paused long enough to let the screen sleep.
    private static boolean castPaused = false;
    private static boolean castPausedLong = false;
    private static boolean television = false;
    private static final long PAUSED_SCREEN_GRACE_MS = 5 * 60_000;
    private static final Runnable pausedScreenGrace = () -> {
        castPausedLong = true;
        syncKeepScreenOn();
    };

    /// Runs `r` on the main thread, inline when already there.
    private static void onMain(Runnable r) {
        if (Looper.myLooper() == Looper.getMainLooper()) {
            r.run();
        } else {
            handler.post(r);
        }
    }

    /// Starts the process-level half once. Idempotent, main thread.
    @SuppressLint("WakelockTimeout")
    static void ensureStarted(Context ctx) {
        if (started) {
            return;
        }
        started = true;
        app = ctx.getApplicationContext();
        android.app.UiModeManager ui =
                (android.app.UiModeManager) app.getSystemService(Context.UI_MODE_SERVICE);
        television = ui != null && ui.getCurrentModeType()
                == android.content.res.Configuration.UI_MODE_TYPE_TELEVISION;
        nativeCoreInit(app);
        // the Rust core too: a boot start has no activity to start it
        String filesDir = app.getFilesDir().getPath();
        // the installed build's versionCode keys the renderer probe
        nativeCoreStart(filesDir, Updater.installedVersionCode(app));
        startOnBoot = nativeStartOnBoot(filesDir);
        if (startOnBoot == -1 && upgradedFromKotlin(app)) {
            // the Kotlin receiver came up at boot (a tap-to-start notice from
            // 12 on), an upgrade starts on boot without asking
            Log.i(TAG, "a Kotlin upgrade or a lost earlier yes, start on boot on");
            startOnBoot = 1;
            nativeSetStartOnBoot(true);
            NotificationManager nm = app.getSystemService(NotificationManager.class);
            nm.deleteNotificationChannel(KOTLIN_SERVICE_CHANNEL);
            nm.deleteNotificationChannel("BootReceiverServiceChannel");
        }
        syncBootComponent(startOnBoot == 1);
        if (startOnBoot == 1 && !(ctx instanceof ReceiverService)) {
            requestService();
        }

        createMediaSession();
        ReceiverService.ensureChannel(app);

        netThread = new HandlerThread("fcast-net");
        netThread.start();
        netHandler = new Handler(netThread.getLooper());

        nsdManager = (NsdManager) app.getSystemService(Context.NSD_SERVICE);

        // Fills {hostname} and the default names. Settings apply on restart,
        // so reading them once here is enough.
        String hostname = deviceHostname(app);
        String[] names = nativeServiceNames(app.getFilesDir().getPath(), hostname);
        if (names == null || names.length < 1) {
            // An unreadable config is an unset one: FCast enabled under the
            // default name, as rcore's advertised_fcast_name resolves it.
            Log.w(TAG, "service name unavailable from native, advertising the default");
            names = new String[] { "FCast-" + hostname };
        }
        fcastServiceName = names[0] == null ? null : truncateUtf8(names[0], 63);

        setMdnsDeviceName(fcastServiceName != null ? fcastServiceName
                : truncateUtf8("FCast-" + hostname, 63));
        registerServices();

        connectivityManager = (ConnectivityManager) app.getSystemService(Context.CONNECTIVITY_SERVICE);
        NetworkRequest networkRequest = new NetworkRequest.Builder()
                .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
                .addTransportType(NetworkCapabilities.TRANSPORT_ETHERNET)
                .build();
        connectivityManager.registerNetworkCallback(networkRequest, new NetworkCallbackHandler());

        netHandler.post(ReceiverCore::sweepAddresses);
        handler.postDelayed(periodicSweep, SWEEP_INTERVAL_MS);

        // Created here; updateWifiLockMode owns hold and mode from now on.
        // Non ref-counted so repeated acquires are idempotent and one
        // release always drops the lock.
        wifiManager = (WifiManager) app.getSystemService(Context.WIFI_SERVICE);
        updateWifiLockMode();
        // Discovery has to survive the screen going off (see MulticastLease).
        MulticastLease.acquire(app);

        PowerManager powerManager = (PowerManager) app.getSystemService(Context.POWER_SERVICE);
        cpuWakeLock = powerManager.newWakeLock(PowerManager.PARTIAL_WAKE_LOCK, "FCastRsReceiver:WakeLock");
        cpuWakeLock.setReferenceCounted(false);
    }

    // --- the activity ------------------------------------------------------

    static void attachActivity(MainActivity a) {
        activity = new WeakReference<>(a);
        // a new window starts without the flag a running cast wants
        a.applyKeepScreenOn(keepScreenOn());
        a.applyCastWake(castWake, television);
    }

    /// Visual casts pin the screen, audio ones too on a TV, where the audio
    /// usually plays through the screen. A long pause lets it sleep.
    private static boolean keepScreenOn() {
        return castActive && (castVisual || television) && !castPausedLong;
    }

    private static void syncKeepScreenOn() {
        MainActivity a = activity.get();
        if (a != null) {
            a.applyKeepScreenOn(keepScreenOn());
        }
    }

    /// The player paused or left pause, from native code. Any thread.
    static void setCastPaused(boolean paused) {
        onMain(() -> {
            if (paused == castPaused) {
                return;
            }
            castPaused = paused;
            castPausedLong = false;
            handler.removeCallbacks(pausedScreenGrace);
            if (paused) {
                // the screen times out at once when the flag drops, the grace
                // keeps short pauses lit
                handler.postDelayed(pausedScreenGrace, PAUSED_SCREEN_GRACE_MS);
            }
            // No wake on resume: ACQUIRE_CAUSES_WAKEUP needs the privileged
            // TURN_SCREEN_ON from 34, and a wake can power the TV over CEC
            syncKeepScreenOn();
        });
    }

    static void detachActivity(MainActivity a) {
        if (activity.get() == a) {
            activity = new WeakReference<>(null);
        }
    }

    /// Activity start/stop.
    static void setUiVisible(boolean visible) {
        uiVisible = visible;
        if (visible) {
            handler.removeCallbacks(castWaitingCheck);
            ReceiverService.cancelCastWaiting(app);
            // in the foreground a refused type switch goes through
            ReceiverService.retrySwitchIfNeeded(false);
        }
        updateWifiLockMode();
    }

    static boolean castVisual() {
        return castVisual;
    }

    // Set when the activity asked for the service: its onStartCommand comes
    // later on this thread, after an onDestroy that may follow onStop at once.
    private static volatile boolean serviceWanted = false;

    // 1 on, 0 off, -1 never asked (first launch prompts, off until answered)
    private static volatile int startOnBoot = -1;

    /// The service runs for the life of the process, not only backgrounded.
    static boolean startOnBoot() {
        return startOnBoot == 1;
    }

    static boolean startOnBootUnasked() {
        return startOnBoot == -1;
    }

    /// The first-launch answer. The core saves it and applies it back
    /// through applyStartOnBoot, as for a drawer edit.
    static void answerStartOnBoot(boolean on) {
        startOnBoot = on ? 1 : 0;
        nativeSetStartOnBoot(on);
    }

    /// The setting changed (drawer or prompt), from native code on any
    /// thread. Takes effect at once: no reboot needed to be reachable.
    static void applyStartOnBoot(boolean on) {
        onMain(() -> {
            startOnBoot = on ? 1 : 0;
            syncBootComponent(on);
            if (on) {
                requestService();
            } else if (uiVisible) {
                // back to the service only while backgrounded
                serviceWanted = false;
                app.stopService(new Intent(app, ReceiverService.class));
            }
            ReceiverService.refreshIfRunning();
        });
    }

    // Created by every run of the Kotlin receiver's service.
    private static final String KOTLIN_SERVICE_CHANNEL = "NetworkListenerServiceChannel";

    /// An install this app never started (every start sets the boot
    /// component explicitly) that the Kotlin receiver ran in. Or one whose
    /// upgrade start died before the answer reached the config: that start
    /// already enabled the component and deleted the channel, and only it
    /// leaves the component on with the question unanswered.
    private static boolean upgradedFromKotlin(Context ctx) {
        android.content.pm.PackageManager pm = ctx.getPackageManager();
        android.content.ComponentName boot = new android.content.ComponentName(ctx, BootReceiver.class);
        int state = pm.getComponentEnabledSetting(boot);
        if (state == android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_ENABLED) {
            return true;
        }
        return state == android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_DEFAULT
                && ctx.getSystemService(NotificationManager.class)
                        .getNotificationChannel(KOTLIN_SERVICE_CHANNEL) != null;
    }

    private static void syncBootComponent(boolean on) {
        android.content.pm.PackageManager pm = app.getPackageManager();
        android.content.ComponentName boot = new android.content.ComponentName(app, BootReceiver.class);
        int want = on
                ? android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_ENABLED
                : android.content.pm.PackageManager.COMPONENT_ENABLED_STATE_DISABLED;
        if (pm.getComponentEnabledSetting(boot) != want) {
            pm.setComponentEnabledSetting(boot, want, android.content.pm.PackageManager.DONT_KILL_APP);
        }
    }

    private static void requestService() {
        try {
            app.startForegroundService(new Intent(app, ReceiverService.class));
            serviceWanted = true;
        } catch (Exception e) {
            Log.w(TAG, "foreground service refused", e);
        }
    }

    /// The UI could not start on the renderer it probed, the next activity
    /// takes the fallback (receiver-android's choose_renderer). From native
    /// code as the failed activity finishes, delayed past that finish so the
    /// singleTask MainActivity comes up as a fresh instance.
    static void relaunchUi() {
        handler.postDelayed(() -> {
            Intent launch = new Intent(app, SplashActivity.class);
            launch.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
            try {
                app.startActivity(launch);
            } catch (Exception e) {
                Log.w(TAG, "relaunch after the renderer fallback refused", e);
            }
        }, 500);
    }

    /// The notification's Quit: the receiver ends, whatever owns it.
    static void quit() {
        Log.i(TAG, "quit from the notification");
        stopServiceForExit();
        nativeShutdown();
    }

    /// The process is about to exit on purpose. The service is sticky, so it
    /// must stop first or the system restarts it as after a kill. Any thread.
    static void stopServiceForExit() {
        serviceWanted = false;
        if (app != null) {
            app.stopService(new Intent(app, ReceiverService.class));
        }
    }

    static void setServiceWanted(boolean wanted) {
        serviceWanted = wanted;
    }

    /// Whether the process must outlive a destroyed activity: the service is
    /// what keeps the receiver reachable without one. From native code.
    static boolean keepAlive() {
        return serviceWanted || ReceiverService.isRunning();
    }

    /// The service stopped. With no activity left nothing owns the process.
    static void onServiceStopped() {
        if (started && activity.get() == null) {
            Log.i(TAG, "service stopped with no activity, ending the receiver");
            nativeShutdown();
        }
    }

    // --- discovery -----------------------------------------------------------

    /// NsdManager removes a listener BEFORE delivering onRegistrationFailed,
    /// so unregistering it afterwards throws; every outcome is handled on
    /// the main handler because the callbacks arrive on the connectivity
    /// thread.
    static final class NsdListener implements NsdManager.RegistrationListener {
        final String label;

        NsdListener(String label) {
            this.label = label;
        }

        @Override
        public void onRegistrationFailed(NsdServiceInfo info, int errorCode) {
            handler.post(() -> {
                // the platform already dropped this listener
                if (fcastReg == this) {
                    fcastReg = null;
                }
                if (fcastRetryPending) {
                    return;
                }
                fcastRetryPending = true;
                fcastRetries += 1;
                long delay = 3_000L << Math.min(fcastRetries - 1, 4);
                Log.e(TAG, label + " registration failed: " + errorCode
                        + ", retry " + fcastRetries + " in " + delay + "ms");
                handler.postDelayed(() -> {
                    fcastRetryPending = false;
                    if (fcastReg == null) {
                        fcastPollGen += 1;
                        registerFCastWhenReady(fcastPollGen);
                    }
                }, delay);
            });
        }

        @Override
        public void onUnregistrationFailed(NsdServiceInfo info, int errorCode) {
            Log.e(TAG, label + " unregistration failed: " + errorCode);
        }

        @Override
        public void onServiceRegistered(NsdServiceInfo info) {
            Log.i(TAG, label + " registered as " + info.getServiceName());
            handler.post(() -> {
                fcastRetries = 0;
                // The daemon renames on collision. The DISPLAYED name
                // follows the network's truth; the registration base
                // never moves, or renames would compound.
                setMdnsDeviceName(info.getServiceName());
            });
        }

        @Override
        public void onServiceUnregistered(NsdServiceInfo info) { }
    }

    private static void quietUnregister(NsdListener listener) {
        if (listener == null) {
            return;
        }
        try {
            nsdManager.unregisterService(listener);
        } catch (IllegalArgumentException e) {
            // already removed by a failure callback
        }
    }

    /// DNS-SD instance names cap at 63 bytes of utf8.
    private static String truncateUtf8(String s, int maxBytes) {
        byte[] bytes = s.getBytes(StandardCharsets.UTF_8);
        if (bytes.length <= maxBytes) {
            return s;
        }
        while (maxBytes > 0 && (bytes[maxBytes] & 0xC0) == 0x80) {
            maxBytes--;
        }
        return new String(bytes, 0, maxBytes, StandardCharsets.UTF_8);
    }

    /// (Re-)register the fcast service, dropping any prior registration first.
    /// It waits for the TXT records (TLS fingerprint + protocol
    /// version) AND the committed listen port: v4 senders key their secure
    /// connect on the records, and an advertisement pointing at an unbound
    /// port hands early senders a connection refuse.
    private static void registerServices() {
        fcastPollGen += 1;
        quietUnregister(fcastReg);
        fcastReg = null;

        if (fcastServiceName != null) {
            registerFCastWhenReady(fcastPollGen);
        }
    }

    private static void registerFCastWhenReady(int gen) {
        if (gen != fcastPollGen) {
            // a newer registration cycle owns the field now
            return;
        }
        Map<String, String> attrs = new HashMap<>();
        int port = getFCastPort();
        if (port == 0 || !getFCastTxtAttribs(attrs)) {
            handler.postDelayed(() -> registerFCastWhenReady(gen), 200);
            return;
        }
        NsdServiceInfo info = new NsdServiceInfo();
        info.setServiceName(fcastServiceName);
        info.setServiceType("_fcast._tcp");
        info.setPort(port);
        for (Map.Entry<String, String> a : attrs.entrySet()) {
            info.setAttribute(a.getKey(), a.getValue());
        }
        fcastReg = new NsdListener("_fcast");
        nsdManager.registerService(info, NsdManager.PROTOCOL_DNS_SD, fcastReg);
    }

    // --- the network sweep ---------------------------------------------------

    /// The previous sweep's raw addresses, net thread only. An unchanged
    /// sweep is not forwarded: the receiver rebuilds its connection QR on
    /// every address push, and the 30 s timer would otherwise do that for
    /// the app's whole life.
    private static ArrayList<byte[]> lastSweep = null;

    private static boolean sameAddresses(ArrayList<byte[]> a, ArrayList<byte[]> b) {
        if (a.size() != b.size()) {
            return false;
        }
        for (int i = 0; i < a.size(); i++) {
            if (!Arrays.equals(a.get(i), b.get(i))) {
                return false;
            }
        }
        return true;
    }

    /// One authoritative sweep instead of per-Network bookkeeping: every
    /// interface's current addresses, as the full replacement set. Covers
    /// hotspot/tethering interfaces the ConnectivityManager never reports.
    /// Runs on the net thread.
    private static void sweepAddresses() {
        ArrayList<ByteBuffer> addrs = new ArrayList<>();
        ArrayList<byte[]> raw = new ArrayList<>();
        try {
            Enumeration<NetworkInterface> ifaces = NetworkInterface.getNetworkInterfaces();
            while (ifaces != null && ifaces.hasMoreElements()) {
                NetworkInterface iface = ifaces.nextElement();
                if (!iface.isUp() || iface.isLoopback()) {
                    continue;
                }
                Enumeration<InetAddress> ifaceAddrs = iface.getInetAddresses();
                while (ifaceAddrs.hasMoreElements()) {
                    InetAddress addr = ifaceAddrs.nextElement();
                    if (addr.isLoopbackAddress()) {
                        continue;
                    }
                    byte[] addressBytes = addr.getAddress();
                    raw.add(addressBytes);
                    ByteBuffer buf = ByteBuffer.allocateDirect(addressBytes.length);
                    buf.put(addressBytes);
                    addrs.add(buf);
                }
            }
        } catch (java.net.SocketException e) {
            Log.e(TAG, "interface sweep failed", e);
            return;
        }
        if (lastSweep != null && sameAddresses(lastSweep, raw)) {
            return;
        }
        lastSweep = raw;
        Log.d(TAG, "address sweep: " + addrs.size() + " addresses");
        nativeSetAddresses(addrs);
    }

    /// Debounced reaction to any connectivity signal: re-sweep addresses and
    /// re-register NSD, since the platform responder's registrations go
    /// stale across interface changes.
    private static final Runnable networkSettled = () -> {
        netHandler.post(ReceiverCore::sweepAddresses);
        registerServices();
    };

    private static void onNetworkChanged() {
        handler.removeCallbacks(networkSettled);
        handler.postDelayed(networkSettled, NETWORK_SETTLE_MS);
    }

    private static final Runnable periodicSweep = new Runnable() {
        @Override
        public void run() {
            netHandler.post(ReceiverCore::sweepAddresses);
            handler.postDelayed(this, SWEEP_INTERVAL_MS);
        }
    };

    private static final class NetworkCallbackHandler extends ConnectivityManager.NetworkCallback {
        @Override
        public void onAvailable(@NonNull Network network) {
            handler.post(ReceiverCore::onNetworkChanged);
        }

        @Override
        public void onLost(@NonNull Network network) {
            handler.post(ReceiverCore::onNetworkChanged);
        }

        @Override
        public void onLinkPropertiesChanged(@NonNull Network network,
                @NonNull LinkProperties props) {
            // AP roams, DHCP renews and IPv6 prefix changes keep the same
            // Network and only fire this.
            handler.post(ReceiverCore::onNetworkChanged);
        }
    }

    /// LOW_LATENCY only bites while foreground with the screen on and
    /// silently degrades in the background, exactly where a backgrounded
    /// cast needs wifi kept awake: swap modes on the visibility edge.
    private static void updateWifiLockMode() {
        // null on a device without wifi, an ethernet-only box
        if (wifiManager == null) {
            return;
        }
        boolean foreground = uiVisible;
        int mode = (foreground && android.os.Build.VERSION.SDK_INT >= 29)
                ? WifiManager.WIFI_MODE_FULL_LOW_LATENCY
                : WifiManager.WIFI_MODE_FULL_HIGH_PERF;
        // An idle receiver must answer the moment a sender connects, so the
        // lock is held whenever the activity is up, not only while casting.
        // Without it the radio power saves and LAN round trips balloon to
        // ~500ms, blowing sender handshake deadlines (seen on the LEAP-S1).
        // Backgrounded, only a running cast justifies keeping it.
        boolean want = foreground || castActive;
        if (wifiLock == null || mode != wifiLockMode) {
            if (wifiLock != null && wifiLock.isHeld()) {
                wifiLock.release();
            }
            wifiLock = wifiManager.createWifiLock(mode, "FCastRsReceiver:WifiLock");
            wifiLock.setReferenceCounted(false);
            wifiLockMode = mode;
        }
        if (want && !wifiLock.isHeld()) {
            wifiLock.acquire();
        } else if (!want && wifiLock.isHeld()) {
            wifiLock.release();
        }
    }

    // --- playback resources --------------------------------------------------

    private static android.media.AudioFocusRequest focusRequest = null;
    /// The request is on the stack but not granted yet (a call is on).
    private static boolean focusDelayed = false;
    private static android.content.BroadcastReceiver noisyReceiver = null;

    private static final AudioManager.OnAudioFocusChangeListener focusListener = change -> {
        switch (change) {
            case AudioManager.AUDIOFOCUS_LOSS:
                // The system already removed this app from the focus stack;
                // dropping the request here is what makes the next resume
                // re-request instead of playing focusless over another app.
                onMain(() -> {
                    if (focusRequest != null) {
                        AudioManager am = (AudioManager) app.getSystemService(Context.AUDIO_SERVICE);
                        am.abandonAudioFocusRequest(focusRequest);
                        focusRequest = null;
                    }
                    focusDelayed = false;
                });
                nativeAudioEvent(0);
                break;
            case AudioManager.AUDIOFOCUS_LOSS_TRANSIENT:
                nativeAudioEvent(1);
                break;
            case AudioManager.AUDIOFOCUS_LOSS_TRANSIENT_CAN_DUCK:
                // the system ducks us (API 26+), a chime must not pause the cast
                break;
            case AudioManager.AUDIOFOCUS_GAIN:
                onMain(() -> focusDelayed = false);
                nativeAudioEvent(2);
                break;
        }
    };

    /// Request focus if none is held. Called from native code on every
    /// play/load edge: a cast arriving during a phone call must not play
    /// over it, and after a permanent loss the request is gone and needs
    /// remaking. A delayed grant (during a call) pauses as a transient loss
    /// and resumes on the gain that follows. A refusal only pauses.
    static void ensureAudioFocus() {
        onMain(() -> {
            if (focusRequest != null) {
                // still waiting on the call, a resume or a new item pauses again
                if (focusDelayed) {
                    nativeAudioEvent(1);
                }
                return;
            }
            AudioManager am = (AudioManager) app.getSystemService(Context.AUDIO_SERVICE);
            android.media.AudioFocusRequest req =
                    new android.media.AudioFocusRequest.Builder(AudioManager.AUDIOFOCUS_GAIN)
                            .setAudioAttributes(new android.media.AudioAttributes.Builder()
                                    .setUsage(android.media.AudioAttributes.USAGE_MEDIA)
                                    .setContentType(android.media.AudioAttributes.CONTENT_TYPE_MOVIE)
                                    .build())
                            .setOnAudioFocusChangeListener(focusListener)
                            .setAcceptsDelayedFocusGain(true)
                            .build();
            int result = am.requestAudioFocus(req);
            if (result == AudioManager.AUDIOFOCUS_REQUEST_GRANTED) {
                focusRequest = req;
            } else if (result == AudioManager.AUDIOFOCUS_REQUEST_DELAYED) {
                // on the stack, the gain arrives through the listener
                focusRequest = req;
                focusDelayed = true;
                Log.i(TAG, "audio focus delayed");
                nativeAudioEvent(1);
            } else {
                Log.i(TAG, "audio focus refused (" + result + ")");
                nativeAudioEvent(1);
            }
        });
    }

    private static MediaSession mediaSession = null;
    // From 33 the media notification builds its buttons from the session's
    // PlaybackState and ignores the notification's own actions. ACTION_STOP
    // gets no button there, a custom action does.
    private static final String CUSTOM_ACTION_STOP = "stop";
    private static volatile android.media.session.PlaybackState.CustomAction stopAction = null;

    /// The name the user gave the device in Settings (what Cast and AirPlay
    /// show as well), else manufacturer and model. Unset on some boxes.
    private static String deviceHostname(Context ctx) {
        String name = android.provider.Settings.Global.getString(
                ctx.getContentResolver(), android.provider.Settings.Global.DEVICE_NAME);
        if (name != null && !name.trim().isEmpty()) {
            return name.trim();
        }
        String modelName;
        if (android.os.Build.MODEL.contains(android.os.Build.MANUFACTURER)) {
            // quoted: a manufacturer string with regex metacharacters would
            // throw out of onCreate on that device
            modelName = android.os.Build.MODEL
                    .replaceFirst("^" + Pattern.quote(android.os.Build.MANUFACTURER), "")
                    .trim();
        } else {
            modelName = android.os.Build.MODEL;
        }
        return android.os.Build.MANUFACTURER + "-" + modelName;
    }

    /// The session: what routes media buttons, drives the lock-screen and
    /// BT transport surfaces, and feeds the notification's MediaStyle.
    private static void createMediaSession() {
        mediaSession = new MediaSession(app, "FCastReceiver");
        mediaSession.setCallback(new MediaSession.Callback() {
            @Override
            public void onPlay() {
                nativeMediaCommand(2);
            }

            @Override
            public void onPause() {
                nativeMediaCommand(1);
            }

            @Override
            public void onStop() {
                nativeMediaCommand(0);
            }

            @Override
            public void onSeekTo(long posMs) {
                nativeMediaSeek(posMs / 1000.0);
            }

            @Override
            public void onCustomAction(@NonNull String action, Bundle extras) {
                if (CUSTOM_ACTION_STOP.equals(action)) {
                    nativeMediaCommand(0);
                }
            }
        });
        mediaSession.setPlaybackToLocal(new android.media.AudioAttributes.Builder()
                .setUsage(android.media.AudioAttributes.USAGE_MEDIA)
                .setContentType(android.media.AudioAttributes.CONTENT_TYPE_MOVIE)
                .build());
        // lock-screen/QS media card tap opens the app
        Intent open = new Intent(app, MainActivity.class);
        open.setFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        mediaSession.setSessionActivity(PendingIntent.getActivity(
                app, 0, open, PendingIntent.FLAG_IMMUTABLE));
        ReceiverService.sessionToken = mediaSession.getSessionToken();
    }

    /// The CPU wake lock follows actually-playing, not merely
    /// session-active: a cast paused overnight with the screen off would
    /// otherwise hold a partial wake lock for hours, which is exactly what
    /// Play's excessive-wake-lock quality threshold flags. Playing audio
    /// under the mediaPlayback FGS is the policy's exempt case.
    @SuppressLint("WakelockTimeout")
    private static void syncWakeLock() {
        boolean want = castActive && castPlaying;
        if (want && !cpuWakeLock.isHeld()) {
            cpuWakeLock.acquire();
        } else if (!want && cpuWakeLock.isHeld()) {
            cpuWakeLock.release();
        }
    }

    /// Called from native code on state edges and a 1 Hz keepalive.
    /// Any thread; MediaSession is thread-safe.
    static void updateMediaSession(boolean playing, long positionMs, float speed) {
        onMain(() -> {
            castPlaying = playing;
            syncWakeLock();
        });
        MediaSession session = mediaSession;
        if (session == null) {
            return;
        }
        android.media.session.PlaybackState.Builder b =
                new android.media.session.PlaybackState.Builder()
                        .setActions(android.media.session.PlaybackState.ACTION_PLAY
                                | android.media.session.PlaybackState.ACTION_PAUSE
                                | android.media.session.PlaybackState.ACTION_PLAY_PAUSE
                                | android.media.session.PlaybackState.ACTION_STOP
                                | android.media.session.PlaybackState.ACTION_SEEK_TO)
                        // paused state must not extrapolate: the platform
                        // advances position at the given speed
                        .setState(playing
                                        ? android.media.session.PlaybackState.STATE_PLAYING
                                        : android.media.session.PlaybackState.STATE_PAUSED,
                                positionMs, playing ? speed : 0f);
        if (android.os.Build.VERSION.SDK_INT >= 33) {
            android.media.session.PlaybackState.CustomAction stop = stopAction;
            if (stop == null) {
                // built once, this runs at 1 Hz on any thread and a racing
                // double build is harmless
                stop = new android.media.session.PlaybackState.CustomAction.Builder(
                        CUSTOM_ACTION_STOP, app.getString(R.string.notification_stop),
                        R.drawable.ic_stat_stop).build();
                stopAction = stop;
            }
            b.addCustomAction(stop);
        }
        session.setPlaybackState(b.build());
    }

    /// Called from native code when the item's title or duration changes.
    static void updateMediaMetadata(String title, long durationMs) {
        MediaSession session = mediaSession;
        if (session != null) {
            session.setMetadata(new android.media.MediaMetadata.Builder()
                    .putString(android.media.MediaMetadata.METADATA_KEY_TITLE, title)
                    .putLong(android.media.MediaMetadata.METADATA_KEY_DURATION, durationMs)
                    .build());
        }
        // the notification shows the title only, a duration change (every
        // tick on a live stream) must not rebuild it
        boolean titleChanged = !java.util.Objects.equals(ReceiverService.castTitle, title);
        ReceiverService.castTitle = title;
        if (titleChanged) {
            ReceiverService.refreshIfRunning();
        } else {
            // a type switch refused from the background is retried here
            ReceiverService.retrySwitchIfNeeded(true);
        }
    }

    /// The single owner of every playback-scoped device resource, called
    /// from native code on the playback active/idle edge. Any thread.
    /// Ordering is preserved by posting to the main looper.
    ///
    /// `audible` is false for images and for items with no audio track:
    /// those take no audio focus (a photo must not pause whatever else is
    /// playing), no media session and no CPU wake lock. The wifi lock follows
    /// `active` and `visual`, the screen pin keepScreenOn.
    @SuppressLint("WakelockTimeout")
    static void setPlaybackActive(boolean active, boolean visual, boolean audible) {
        onMain(() -> {
            // Rising edge only: native calls again when visual or audible
            // settle (the video stream shows up ~60ms into a load), and each
            // call used to send a second start request.
            if (active) {
                // the next item arrived, the ended one keeps the window
                handler.removeCallbacks(endedCastLeave);
            }
            if (active && !castActive && !uiVisible) {
                castArrived();
            } else if (!active) {
                handler.removeCallbacks(castWaitingCheck);
                ReceiverService.cancelCastWaiting(app);
                setCastWake(false);
            }
            boolean audio = active && audible;
            if (mediaSession != null) {
                mediaSession.setActive(audio);
            }
            ReceiverService.castActive = active;
            ReceiverService.refreshIfRunning();
            castVisual = active && visual;

            castActive = active;
            if (!active) {
                castPaused = false;
                castPausedLong = false;
                handler.removeCallbacks(pausedScreenGrace);
            }
            syncKeepScreenOn();
            // For an audible item playing usually follows within a tick;
            // acquiring here too covers the load window, syncWakeLock drops
            // it on pause. A silent item never needs the CPU awake.
            castPlaying = audio;
            syncWakeLock();
            // Not a plain release on the idle edge: a visible idle receiver
            // keeps the lock so the next sender's handshake still lands fast.
            updateWifiLockMode();

            if (audio) {
                ensureAudioFocus();
                if (noisyReceiver == null) {
                    noisyReceiver = new android.content.BroadcastReceiver() {
                        @Override
                        public void onReceive(Context context, Intent intent) {
                            nativeAudioEvent(3);
                        }
                    };
                    if (android.os.Build.VERSION.SDK_INT >= 33) {
                        app.registerReceiver(noisyReceiver,
                                new android.content.IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY),
                                Context.RECEIVER_NOT_EXPORTED);
                    } else {
                        app.registerReceiver(noisyReceiver,
                                new android.content.IntentFilter(AudioManager.ACTION_AUDIO_BECOMING_NOISY));
                    }
                }
            } else {
                if (focusRequest != null) {
                    AudioManager am = (AudioManager) app.getSystemService(Context.AUDIO_SERVICE);
                    am.abandonAudioFocusRequest(focusRequest);
                    focusRequest = null;
                }
                focusDelayed = false;
                if (noisyReceiver != null) {
                    app.unregisterReceiver(noisyReceiver);
                    noisyReceiver = null;
                }
            }
        });
    }

    // --- bringing a cast forward ---------------------------------------------

    // Long enough for a permitted bring-to-front to land first.
    private static final long CAST_WAITING_DELAY_MS = 1500;
    private static final Runnable castWaitingCheck = () -> {
        if (!uiVisible && castActive) {
            ReceiverService.notifyCastWaiting(app);
        }
    };

    /// Come forward, or post tap-to-play if the start is refused.
    private static void castArrived() {
        castFromBackground = true;
        // before the start, an existing window resumes on it
        setCastWake(true);
        bringToFront();
        handler.removeCallbacks(castWaitingCheck);
        handler.postDelayed(castWaitingCheck, CAST_WAITING_DELAY_MS);
    }

    /// Waking the screen is all an app can do about a TV that is off: a TV box
    /// runs CEC One Touch Play on any wake, which powers the TV and switches
    /// its input. Per cast, a window that resumes later for any other reason
    /// (a relaunch, the user) must not wake it.
    private static void setCastWake(boolean on) {
        castWake = on;
        MainActivity a = activity.get();
        if (a != null) {
            a.applyCastWake(on, television);
        }
    }

    /// The window has focus, the screen is on. From MainActivity.
    static void castWakeDone() {
        if (castWake) {
            setCastWake(false);
        }
    }

    /// A load over an active cast, which raises no rising edge in
    /// setPlaybackActive (loads never pass through Idle). Native calls it
    /// only when the load sent no rising edge, so one load brings the
    /// receiver forward once. Any thread, posted behind the
    /// setPlaybackActive it follows.
    static void castLoaded() {
        onMain(() -> {
            if (castActive && !uiVisible) {
                castArrived();
            }
        });
    }

    /// How long an item's end waits before leaving, for a sender that loads
    /// the next item when it hears of the end (desktop sender playlists).
    private static final long ENDED_LEAVE_DELAY_MS = 3000;
    /// A stop's wait, the core's item-boundary hold (presentation.rs
    /// END_HOLD), so a stop followed by a play never leaves and comes back.
    private static final long STOPPED_LEAVE_DELAY_MS = 1000;
    private static final Runnable endedCastLeave = () -> {
        // a new cast took over, it keeps the window and the flag
        if (!castActive) {
            leaveEndedCast();
        }
    };

    /// The cast ended for real, a stop or the last item's end. A UI in PiP
    /// leaves it, and so does one the cast brought forward from the
    /// background, after a grace a new item cancels (longer for an end a
    /// sender may follow up on, `finished`). Native keeps the ended item on
    /// screen until the window is gone. Any thread.
    static void castEnded(boolean finished) {
        onMain(() -> {
            handler.removeCallbacks(endedCastLeave);
            handler.postDelayed(endedCastLeave,
                    finished ? ENDED_LEAVE_DELAY_MS : STOPPED_LEAVE_DELAY_MS);
            MainActivity a = activity.get();
            if (a != null && a.leavesForEndedCast(castFromBackground)) {
                nativeLeavingForEndedCast();
            }
        });
    }

    private static void leaveEndedCast() {
        boolean toBack = castFromBackground;
        castFromBackground = false;
        MainActivity a = activity.get();
        if (a != null) {
            a.leaveForEndedCast(toBack);
        }
    }

    /// Refresh-rate matching for the window showing the cast. Any thread.
    static void setContentFrameRate(float fps) {
        onMain(() -> {
            MainActivity a = activity.get();
            if (a != null) {
                a.applyContentFrameRate(fps);
            }
        });
    }

    /// A cast arriving while backgrounded should put the receiver on screen,
    /// the whole point of a TV cast target. Background activity starts need
    /// the overlay permission (the Kotlin receiver ships the same way,
    /// verified on Android 14); without it the start is refused silently and
    /// castWaitingCheck posts a tap-to-play instead.
    ///
    /// From 34 a PendingIntent carries neither side's start privilege unless
    /// opted in, and from targetSdk 35 that includes the creator, so both
    /// sides opt in explicitly. ALLOWED is deprecated at 36, split into
    /// ALLOW_IF_VISIBLE and ALLOW_ALWAYS, and a backgrounded receiver is
    /// never the visible one.
    private static void bringToFront() {
        Intent i = new Intent(app, MainActivity.class);
        i.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK | Intent.FLAG_ACTIVITY_SINGLE_TOP);
        try {
            if (android.provider.Settings.canDrawOverlays(app)) {
                if (android.os.Build.VERSION.SDK_INT >= 34) {
                    int mode = android.os.Build.VERSION.SDK_INT >= 36
                            ? android.app.ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOW_ALWAYS
                            : android.app.ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED;
                    Bundle creator = android.app.ActivityOptions.makeBasic()
                            .setPendingIntentCreatorBackgroundActivityStartMode(mode)
                            .toBundle();
                    Bundle sender = android.app.ActivityOptions.makeBasic()
                            .setPendingIntentBackgroundActivityStartMode(mode)
                            .toBundle();
                    // CANCEL_CURRENT, not UPDATE_CURRENT: a matching record
                    // keeps the options it was first made with, so one made
                    // without them would never opt in.
                    PendingIntent.getActivity(app, 2, i,
                            PendingIntent.FLAG_CANCEL_CURRENT | PendingIntent.FLAG_IMMUTABLE,
                            creator)
                            .send(sender);
                } else {
                    PendingIntent.getActivity(app, 2, i,
                            PendingIntent.FLAG_UPDATE_CURRENT | PendingIntent.FLAG_IMMUTABLE)
                            .send();
                }
            } else {
                // blocked from the background on 10+, works when the app is
                // merely covered rather than stopped
                app.startActivity(i);
            }
        } catch (Exception e) {
            Log.w(TAG, "bring-to-front refused", e);
        }
    }

    // --- self-update (Updater), from native code on any thread ---------------

    static int updaterMode() {
        return Updater.mode(app);
    }

    static long installedVersionCode() {
        return Updater.installedVersionCode(app);
    }

    static String prepareUpdateDownload() {
        return Updater.prepareDownload(app);
    }

    static void installUpdate(String path, long versionCode) {
        Updater.install(app, path, versionCode);
    }

    // --- natives -------------------------------------------------------------

    /// Hands native code the VM, this class and the app context, from a
    /// thread whose class loader can see the app's classes.
    private static native void nativeCoreInit(Context app);
    /// Quits the receiver and ends the process.
    private static native void nativeShutdown();
    /// Starts the Rust core once per process, with the config in `filesDir`.
    /// `versionCode` is the installed build's, -1 when it cannot be read.
    private static native void nativeCoreStart(String filesDir, long versionCode);
    /// The start-on-boot setting: 1 on, 0 off, -1 never asked.
    private static native int nativeStartOnBoot(String filesDir);
    private static native void nativeSetStartOnBoot(boolean on);
    private static native void nativeSetAddresses(List<ByteBuffer> addrs);
    private static native void setMdnsDeviceName(String name);
    private static native String[] nativeServiceNames(String filesDir, String hostname);
    private static native boolean getFCastTxtAttribs(Map<String, String> attrs);
    private static native int getFCastPort();
    /// Codes: 0 loss, 1 transient loss, 2 gain, 3 becoming noisy. The pause
    /// and resume policy lives in native code, which knows the player state.
    static native void nativeAudioEvent(int code);
    /// Transport commands from the MediaSession and the notification:
    /// 0 stop, 1 pause, 2 resume.
    static native void nativeMediaCommand(int code);
    /// Absolute seek from the session (lock screen, BT remote), seconds.
    static native void nativeMediaSeek(double seconds);
    /// An ended cast will send the task to the back: the core holds the
    /// ended item on screen until onStop instead of showing the idle screen.
    private static native void nativeLeavingForEndedCast();
}
