package org.fcast.rsreceiver.android;

import android.app.Activity;
import android.content.Intent;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.view.View;
import android.view.ViewTreeObserver;

/**
 * Branded launch splash. MainActivity is a NativeActivity whose window is
 * translucent from the first frame (for the video hole punch), so it draws its
 * Surface directly with no view hierarchy and the platform shows no starting
 * window or splash for it, leaving a black cold-start gap that reads as a crash
 * on slow TV hardware.
 *
 * This activity is opaque with a real content view painting the splash art, so
 * it holds the screen while it is up. It launches MainActivity after a short
 * beat (letting its own window draw first) and lingers behind the translucent
 * MainActivity, showing through until the receiver paints its idle screen.
 */
public class SplashActivity extends Activity {
    // A short buffer after the splash's first draw so the frame is actually
    // presented before MainActivity's window comes up, guarding pre-emption
    // without the old fixed lead-in.
    private static final long PRESENT_BUFFER_MS = 120;
    // Retire after MainActivity has had time to paint over us. Its cold start
    // measured ~2s on the slowest box, so leave headroom.
    private static final long RETIRE_MS = 3000;

    private final Handler handler = new Handler(Looper.getMainLooper());

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        if (savedInstanceState != null) {
            // Recreated or returned to: MainActivity owns the screen, retire.
            finish();
            return;
        }

        // A plain view carrying the splash art. Guarantees a drawn frame, so
        // the window is shown rather than held as an empty starting window that
        // the eager MainActivity launch could pre-empt.
        final View splash = new View(this);
        splash.setBackgroundResource(R.drawable.splash_window);
        setContentView(splash);

        // Hand off the moment the splash has actually painted, not on a fixed
        // timer. The pre-draw fires once the first frame is ready; a small
        // buffer lets the compositor present it, then MainActivity launches.
        splash.getViewTreeObserver().addOnPreDrawListener(
                new ViewTreeObserver.OnPreDrawListener() {
                    @Override
                    public boolean onPreDraw() {
                        splash.getViewTreeObserver().removeOnPreDrawListener(this);
                        handler.postDelayed(SplashActivity.this::launchMain, PRESENT_BUFFER_MS);
                        return true;
                    }
                });
        handler.postDelayed(this::finish, RETIRE_MS);
    }

    private void launchMain() {
        Intent intent = new Intent(this, MainActivity.class);
        intent.addFlags(Intent.FLAG_ACTIVITY_NO_ANIMATION);
        startActivity(intent);
    }

    // Back before the handoff completes retires the splash instead of leaving a
    // relaunch trampoline in the stack.
    @Override
    public void onBackPressed() {
        handler.removeCallbacksAndMessages(null);
        finish();
    }
}
