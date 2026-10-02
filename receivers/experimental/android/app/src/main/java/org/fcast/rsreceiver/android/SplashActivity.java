package org.fcast.rsreceiver.android;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.Choreographer;
import android.view.View;
import android.view.ViewTreeObserver;
import java.lang.ref.WeakReference;

/**
 * Branded launch splash. MainActivity is a NativeActivity whose window is
 * translucent from the first frame (for the video hole punch), so it draws its
 * Surface directly with no view hierarchy and the platform shows no starting
 * window or splash for it, leaving a black cold-start gap that reads as a crash
 * on slow TV hardware.
 *
 * This activity is opaque with a real content view painting the splash art. It
 * launches MainActivity once its own frame is submitted and lingers behind the
 * translucent MainActivity until the receiver reports its first frame (retire).
 */
public class SplashActivity extends Activity {
    // A window that never commits a frame still hands off.
    private static final long LAUNCH_BACKSTOP_MS = 1000;
    // A receiver that never paints (crash, failed gst registration).
    private static final long RETIRE_BACKSTOP_MS = 10_000;

    private static WeakReference<SplashActivity> current = new WeakReference<>(null);

    private final Handler handler = new Handler(Looper.getMainLooper());
    private boolean launched = false;

    /// The receiver presented its first frame, the splash has nothing left to
    /// cover. Main thread only.
    static void retire() {
        SplashActivity splash = current.get();
        current = new WeakReference<>(null);
        if (splash != null && !splash.isFinishing()) {
            splash.finish();
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        if (savedInstanceState != null) {
            // Recreated or returned to: MainActivity owns the screen, retire.
            finish();
            return;
        }

        // Warm: the receiver is up and has drawn, there is nothing to cover.
        if (MainActivity.painted) {
            launchMain();
            finish();
            return;
        }

        // A plain view carrying the splash art. Guarantees a drawn frame, so
        // the window is shown rather than held as an empty starting window that
        // the eager MainActivity launch could pre-empt.
        final View splash = new View(this);
        splash.setBackgroundResource(R.drawable.splash_window);
        setContentView(splash);
        current = new WeakReference<>(this);

        // Hand off once the splash frame is submitted, not on a timer.
        if (android.os.Build.VERSION.SDK_INT >= 29) {
            splash.getViewTreeObserver().registerFrameCommitCallback(this::launchMain);
        } else {
            // Frame N+1 starting means the pre-drawn frame N was submitted.
            splash.getViewTreeObserver().addOnPreDrawListener(
                    new ViewTreeObserver.OnPreDrawListener() {
                        @Override
                        public boolean onPreDraw() {
                            splash.getViewTreeObserver().removeOnPreDrawListener(this);
                            Choreographer.getInstance().postFrameCallback(t -> launchMain());
                            return true;
                        }
                    });
        }
        handler.postDelayed(this::launchMain, LAUNCH_BACKSTOP_MS);
        handler.postDelayed(this::finish, RETIRE_BACKSTOP_MS);
    }

    private void launchMain() {
        if (launched || isFinishing()) {
            return;
        }
        launched = true;
        Intent intent = new Intent(this, MainActivity.class);
        intent.addFlags(Intent.FLAG_ACTIVITY_NO_ANIMATION);
        startActivity(intent);
    }

    @Override
    protected void onDestroy() {
        handler.removeCallbacksAndMessages(null);
        if (current.get() == this) {
            current = new WeakReference<>(null);
        }
        super.onDestroy();
    }

    // Back before the handoff completes retires the splash instead of leaving a
    // relaunch trampoline in the stack.
    @Override
    public void onBackPressed() {
        handler.removeCallbacksAndMessages(null);
        finish();
    }
}
