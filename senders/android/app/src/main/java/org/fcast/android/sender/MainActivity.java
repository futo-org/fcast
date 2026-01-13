package org.fcast.android.sender;

import static android.opengl.EGLExt.EGL_OPENGL_ES3_BIT_KHR;
import static android.opengl.GLES11Ext.GL_TEXTURE_EXTERNAL_OES;
import static android.opengl.GLES20.GL_ARRAY_BUFFER;
import static android.opengl.GLES20.GL_CLAMP_TO_EDGE;
import static android.opengl.GLES20.GL_COLOR_ATTACHMENT0;
import static android.opengl.GLES20.GL_COMPILE_STATUS;
import static android.opengl.GLES20.GL_FLOAT;
import static android.opengl.GLES20.GL_FRAGMENT_SHADER;
import static android.opengl.GLES20.GL_FRAMEBUFFER;
import static android.opengl.GLES20.GL_FRAMEBUFFER_COMPLETE;
import static android.opengl.GLES20.GL_LINEAR;
import static android.opengl.GLES20.GL_LINK_STATUS;
import static android.opengl.GLES20.GL_STATIC_DRAW;
import static android.opengl.GLES20.GL_TEXTURE0;
import static android.opengl.GLES20.GL_TEXTURE_2D;
import static android.opengl.GLES20.GL_TEXTURE_MAG_FILTER;
import static android.opengl.GLES20.GL_TEXTURE_MIN_FILTER;
import static android.opengl.GLES20.GL_TEXTURE_WRAP_S;
import static android.opengl.GLES20.GL_TEXTURE_WRAP_T;
import static android.opengl.GLES20.GL_TRIANGLE_STRIP;
import static android.opengl.GLES20.GL_TRUE;
import static android.opengl.GLES20.GL_UNSIGNED_BYTE;
import static android.opengl.GLES20.GL_VERTEX_SHADER;
import static android.opengl.GLES20.glActiveTexture;
import static android.opengl.GLES20.glAttachShader;
import static android.opengl.GLES20.glBindBuffer;
import static android.opengl.GLES20.glBindFramebuffer;
import static android.opengl.GLES20.glBindTexture;
import static android.opengl.GLES20.glBufferData;
import static android.opengl.GLES20.glCheckFramebufferStatus;
import static android.opengl.GLES20.glCompileShader;
import static android.opengl.GLES20.glCreateProgram;
import static android.opengl.GLES20.glCreateShader;
import static android.opengl.GLES20.glDeleteFramebuffers;
import static android.opengl.GLES20.glDeleteProgram;
import static android.opengl.GLES20.glDeleteShader;
import static android.opengl.GLES20.glDeleteTextures;
import static android.opengl.GLES20.glDisableVertexAttribArray;
import static android.opengl.GLES20.glDrawArrays;
import static android.opengl.GLES20.glEnableVertexAttribArray;
import static android.opengl.GLES20.glFinish;
import static android.opengl.GLES20.glFramebufferTexture2D;
import static android.opengl.GLES20.glGenBuffers;
import static android.opengl.GLES20.glGenFramebuffers;
import static android.opengl.GLES20.glGenTextures;
import static android.opengl.GLES20.glGetAttribLocation;
import static android.opengl.GLES20.glGetProgramInfoLog;
import static android.opengl.GLES20.glGetProgramiv;
import static android.opengl.GLES20.glGetShaderInfoLog;
import static android.opengl.GLES20.glGetShaderiv;
import static android.opengl.GLES20.glGetUniformLocation;
import static android.opengl.GLES20.glLinkProgram;
import static android.opengl.GLES20.glShaderSource;
import static android.opengl.GLES20.glTexImage2D;
import static android.opengl.GLES20.glTexParameteri;
import static android.opengl.GLES20.glUniform1i;
import static android.opengl.GLES20.glUniform2f;
import static android.opengl.GLES20.glUniformMatrix4fv;
import static android.opengl.GLES20.glUseProgram;
import static android.opengl.GLES20.glVertexAttribPointer;
import static android.opengl.GLES20.glViewport;
import static android.opengl.GLES30.GL_COLOR_ATTACHMENT1;
import static android.opengl.GLES30.GL_R8;
import static android.opengl.GLES30.GL_RED;
import static android.opengl.GLES30.glDrawBuffers;

import android.app.Activity;
import android.app.NativeActivity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.res.Configuration;
import android.graphics.SurfaceTexture;
import android.hardware.display.DisplayManager;
import android.hardware.display.VirtualDisplay;
import android.media.projection.MediaProjection;
import android.media.projection.MediaProjectionManager;
import android.net.nsd.NsdManager;
import android.net.nsd.NsdServiceInfo;
import android.opengl.EGL14;
import android.opengl.EGLConfig;
import android.opengl.EGLContext;
import android.opengl.EGLDisplay;
import android.opengl.EGLSurface;
import android.os.Build;
import android.os.Bundle;
import android.os.Handler;
import android.os.HandlerThread;
import android.util.DisplayMetrics;
import android.util.Log;
import android.view.Display;
import android.view.Surface;

import androidx.annotation.NonNull;
import androidx.annotation.RequiresApi;
import androidx.localbroadcastmanager.content.LocalBroadcastManager;

import com.journeyapps.barcodescanner.ScanOptions;

import org.freedesktop.gstreamer.GStreamer;

import java.net.Inet6Address;
import java.net.InetAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.FloatBuffer;
import java.nio.IntBuffer;
import java.time.Duration;
import java.time.Instant;
import java.util.List;
import java.util.Objects;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.locks.ReentrantLock;
import java.util.stream.Collectors;

class FCastDiscoveryListener implements NsdManager.DiscoveryListener {
    private static final String TAG = "FCastDiscoveryListener";
    private final NsdManager nsdManager;


    FCastDiscoveryListener(NsdManager nsdManager) {
        this.nsdManager = nsdManager;
    }

    private static ByteBuffer addrConvert(InetAddress addr) {
        byte[] addrB = addr.getAddress();
        int bufLen = addrB.length;
        if (addr.getClass() == Inet6Address.class) {
            bufLen += 4;
        }
        ByteBuffer buffer = ByteBuffer.allocateDirect(bufLen);
        buffer.put(addrB);

        if (addr.getClass() == Inet6Address.class) {
            int scopeId = ((Inet6Address) addr).getScopeId();
            buffer.order(ByteOrder.LITTLE_ENDIAN).putInt(scopeId);
        }

        return buffer;
    }

    @Override
    public void onStartDiscoveryFailed(String serviceType, int errorCode) {
        Log.e(TAG, "Failed to start discovery errorCode=" + errorCode);
    }

    @Override
    public void onStopDiscoveryFailed(String serviceType, int errorCode) {
        Log.e(TAG, "Failed to stop discovery errorCode=" + errorCode);
    }

    @Override
    public void onDiscoveryStarted(String serviceType) {
        Log.i(TAG, "Discovery started");
    }

    @Override
    public void onDiscoveryStopped(String serviceType) {
        Log.i(TAG, "Discovery stopped");
    }

    @Override
    public void onServiceFound(NsdServiceInfo serviceInfo) {
        Log.i(TAG, "Service found serviceInfo=" + serviceInfo);

        List<InetAddress> addrs = List.of();
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            addrs = serviceInfo.getHostAddresses();
        } else {
            InetAddress hostAddr = serviceInfo.getHost();
            if (hostAddr != null) {
                addrs = List.of(hostAddr);
            }
        }
        List<ByteBuffer> addrsB = addrs.stream().map(FCastDiscoveryListener::addrConvert).collect(Collectors.toList());
        serviceFound(serviceInfo.getServiceName(), addrsB, serviceInfo.getPort());

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
            nsdManager.registerServiceInfoCallback(serviceInfo, Runnable::run, new NsdManager.ServiceInfoCallback() {
                @Override
                public void onServiceInfoCallbackRegistrationFailed(int errorCode) {
                }

                @Override
                public void onServiceUpdated(@NonNull NsdServiceInfo serviceInfo) {
                    serviceFound(serviceInfo.getServiceName(), serviceInfo.getHostAddresses().stream().map(FCastDiscoveryListener::addrConvert).collect(Collectors.toList()), serviceInfo.getPort());
                }

                @Override
                public void onServiceLost() {
                    serviceLost(serviceInfo.getServiceName());
                }

                @Override
                public void onServiceInfoCallbackUnregistered() {
                }
            });
        } else {
            nsdManager.resolveService(serviceInfo, new NsdManager.ResolveListener() {
                @Override
                public void onResolveFailed(NsdServiceInfo serviceInfo, int errorCode) {
                    Log.e(TAG, "Service failed to resolve serviceInfo=" + serviceInfo);
                }

                @Override
                public void onServiceResolved(NsdServiceInfo serviceInfo) {
                    Log.i(TAG, "Service resolved serviceInfo=" + serviceInfo);
                    InetAddress addr = serviceInfo.getHost();
                    if (addr != null) {
                        serviceFound(serviceInfo.getServiceName(), List.of(addrConvert(addr)), serviceInfo.getPort());
                    }
                }
            });
        }
    }

    @Override
    public void onServiceLost(NsdServiceInfo serviceInfo) {
        Log.i(TAG, "Service lost serviceInfo=" + serviceInfo);
        serviceLost(serviceInfo.getServiceName());
    }

    private native void serviceFound(String name, List<ByteBuffer> addrs, int port);

    private native void serviceLost(String name);
}

class Discoverer {
    public Discoverer(Context context) {
        NsdManager nsdManager = (NsdManager) context.getSystemService(Context.NSD_SERVICE);
        nsdManager.discoverServices("_fcast._tcp", NsdManager.PROTOCOL_DNS_SD, new FCastDiscoveryListener(nsdManager));
    }
}

class Dimensions {
    public int width;
    public int height;

    Dimensions(int width, int height) {
        this.width = width;
        this.height = height;
    }

    public Dimensions scale(Dimensions maxDims) {
        if (width <= maxDims.width && height <= maxDims.height) {
            return new Dimensions(width, height);
        }

        int x = width;
        int y = height;

        if (height > width) {
            int tmp = maxDims.height;
            maxDims.height = maxDims.width;
            maxDims.width = tmp;
        }

        float aspect_ratio = Math.min((float) maxDims.width / (float) x, (float) maxDims.height / (float) y);
        int scaledWidth = (int) ((float) x * aspect_ratio);
        int scaledHeight = (int) ((float) y * aspect_ratio);

        // NOTE: make the dims divisible by 4 to make subsampling easier
        int uvWidth = 4 * (scaledWidth / 2 / 4);
        int uvHeight = 4 * (scaledHeight / 2 / 4);

        return new Dimensions(uvWidth * 2, uvHeight * 2);
    }
}

public class MainActivity extends NativeActivity implements DisplayManager.DisplayListener {
    public static final String ACTION_RESULT = "org.fcast.android.sender.SCREEN_CAPTURE_RESULT";
    public static final String ACTION_MEDIA_PROJECTION_STARTED = "org.fcast.android.sender.ACTION_MEDIA_PROJECTION_STARTED";
    private static final int REQUEST_CODE = 1;
    private static final int QR_SCAN_REQUEST_CODE = 2;
    private static final String TAG = "MainActivity";
    private final ReentrantLock captureLock = new ReentrantLock();
    private final CaptureBroadcastReceiver receiver = new CaptureBroadcastReceiver();
    private final float[] quad = { //
            -1f, -1f, 0f, 1f, //
            1f, -1f, 1f, 1f,  //
            -1f, 1f, 0f, 0f,  //
            1f, 1f, 1f, 0f,   //
    };
    Program yProg = null;
    MegaProgram megaProg = null;
    Framebuffer yFramebuffer = null;
    Megabuffer megabuffer = null;
    int vboId;
    Dimensions srcDims = null;
    Dimensions downscaledDims = null;
    Dimensions uvDims = null;
    AtomicBoolean shouldCapture = new AtomicBoolean(false);
    int oesTexId;
    Instant lastFrameSent = Instant.EPOCH;
    private ProjectionCallback projectionCallback;
    private MediaProjectionManager mediaProjectionManager;
    private MediaProjection mediaProjection;
    private VirtualDisplay virtualDisplay;
    private SurfaceTexture surfaceTexture;
    private EGLContext eglContext = EGL14.EGL_NO_CONTEXT;
    private EGLDisplay eglDisplay = EGL14.EGL_NO_DISPLAY;
    private EGLSurface eglSurface = EGL14.EGL_NO_SURFACE;
    private Surface surface;
    private Handler glHandler;
    private DisplayManager displayManager;
    private int userMaxWidth = 1920;
    private int userMaxHeight = 1080;
    private int userMaxFps = 30;
    private long nativeEglCtx = 0;

    static {
        System.loadLibrary("gstreamer_android");
        System.loadLibrary("fcastsender");
    }

    @Override
    public void onDisplayAdded(int displayId) {
    }

    @Override
    public void onDisplayRemoved(int displayId) {
    }

    @Override
    public void onConfigurationChanged(Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
    }

    @Override
    public void onDisplayChanged(int displayId) {
        if (srcDims == null || Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE || this.getWindowManager().getDefaultDisplay().getDisplayId() != displayId) {
            return;
        }

        Display display = displayManager.getDisplay(displayId);
        if (display == null) {
            Log.e(TAG, "Could not get display for displayId=" + displayId);
            return;
        }

        android.graphics.Point newSize = new android.graphics.Point();
        display.getRealSize(newSize);

        if (newSize.x == srcDims.width && newSize.y == srcDims.height) {
            // No change
            return;
        }

        Dimensions newDims = new Dimensions(newSize.x, newSize.y);

        if (shouldCapture.get() && virtualDisplay != null) {
            android.util.DisplayMetrics m = new android.util.DisplayMetrics();
            this.getWindowManager().getDefaultDisplay().getMetrics(m);
            cleanupCapture(false);
            glHandler.post(() -> setupGles(new Dimensions(userMaxWidth, userMaxHeight), newDims));
        }
    }

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);

        try {
            GStreamer.init(this);
        } catch (Exception e) {
            Log.e(TAG, "Failed to init GStreamer ${e}");
            finish();
        }

        Discoverer discoverer = new Discoverer(this);

        projectionCallback = new ProjectionCallback();
        mediaProjectionManager = (MediaProjectionManager) getSystemService(MEDIA_PROJECTION_SERVICE);

        HandlerThread glThread = new HandlerThread("OpenGLThread");
        glThread.start();
        glHandler = new Handler(glThread.getLooper());

        IntentFilter filter = new IntentFilter(ACTION_MEDIA_PROJECTION_STARTED);
        filter.addCategory(Intent.CATEGORY_DEFAULT);
        LocalBroadcastManager.getInstance(this).registerReceiver(receiver, filter);

        displayManager = (DisplayManager) getSystemService(Context.DISPLAY_SERVICE);
        displayManager.registerDisplayListener(this, new Handler(getMainLooper()));
    }

    private void renderToMegaFbWithMegaProg(int oesTexId, Megabuffer fb, MegaProgram prog, float[] texMatrix) {
        glBindFramebuffer(GL_FRAMEBUFFER, fb.fboId);
        glDrawBuffers(2, IntBuffer.wrap(new int[]{GL_COLOR_ATTACHMENT0, GL_COLOR_ATTACHMENT1}));

        // NOTE: div by two here
        glViewport(0, 0, fb.dims.width / 2, fb.dims.height / 2);

        glUseProgram(prog.program);

        glBindBuffer(GL_ARRAY_BUFFER, vboId);

        glEnableVertexAttribArray(prog.position);
        glVertexAttribPointer(prog.position, 2, GL_FLOAT, false, 16, 0);

        glEnableVertexAttribArray(prog.texCoord);
        glVertexAttribPointer(prog.texCoord, 2, GL_FLOAT, false, 16, 8);

        glUniformMatrix4fv(prog.texMatrix, 1, false, texMatrix, 0);

        glUniform2f(prog.srcSize, (float) srcDims.width, (float) srcDims.height);

        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, oesTexId);
        glUniform1i(prog.textureUniform, 0);

        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        glDisableVertexAttribArray(prog.position);
        glDisableVertexAttribArray(prog.texCoord);
        glBindBuffer(GL_ARRAY_BUFFER, 0);

        glBindTexture(GL_TEXTURE_EXTERNAL_OES, 0);

        glBindFramebuffer(GL_FRAMEBUFFER, 0);
    }

    private void renderToFbWithProg(int oesTexId, Framebuffer fb, Program prog, float[] texMatrix) {
        glBindFramebuffer(GL_FRAMEBUFFER, fb.fboId);
        glViewport(0, 0, fb.dims.width, fb.dims.height);

        glUseProgram(prog.program);

        glBindBuffer(GL_ARRAY_BUFFER, vboId);

        glEnableVertexAttribArray(prog.position);
        glVertexAttribPointer(prog.position, 2, GL_FLOAT, false, 16, 0);

        glEnableVertexAttribArray(prog.texCoord);
        glVertexAttribPointer(prog.texCoord, 2, GL_FLOAT, false, 16, 8);

        glUniformMatrix4fv(prog.texMatrix, 1, false, texMatrix, 0);

        glActiveTexture(GL_TEXTURE0);
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, oesTexId);
        glUniform1i(prog.textureUniform, 0);

        glDrawArrays(GL_TRIANGLE_STRIP, 0, 4);

        glDisableVertexAttribArray(prog.position);
        glDisableVertexAttribArray(prog.texCoord);
        glBindBuffer(GL_ARRAY_BUFFER, 0);

        glBindTexture(GL_TEXTURE_EXTERNAL_OES, 0);

        glBindFramebuffer(GL_FRAMEBUFFER, 0);
    }

    private void onFrameAvailable(SurfaceTexture surfaceTexture) throws RuntimeException {
        surfaceTexture.updateTexImage();

        Instant now = Instant.now();
        // Drop early frames
        if (Duration.between(lastFrameSent, now).compareTo(Duration.ofMillis(1000 / userMaxFps)) < 0) {
            return;
        }
        lastFrameSent = now;

        float[] texMatrix = new float[16];
        surfaceTexture.getTransformMatrix(texMatrix);

        renderToFbWithProg(oesTexId, yFramebuffer, yProg, texMatrix);
        renderToMegaFbWithMegaProg(oesTexId, megabuffer, megaProg, texMatrix);

        glFinish();

        nativeProcessFrame(nativeEglCtx, downscaledDims.width, downscaledDims.height, userMaxFps, yFramebuffer.fboId, megabuffer.fboId);
    }

    private void setupGles(Dimensions maxDims, Dimensions suggestedDims) {
        captureLock.lock();

        Log.d(TAG, "Getting EGL eglDisplay");
        eglDisplay = EGL14.eglGetDisplay(EGL14.EGL_DEFAULT_DISPLAY);
        if (eglDisplay == EGL14.EGL_NO_DISPLAY) {
            Log.e(TAG, "No eglDisplay");
            captureLock.unlock();
            throw new RuntimeException("No eglDisplay");
        }

        Log.d(TAG, "Initializing EGL eglDisplay");
        if (!EGL14.eglInitialize(eglDisplay, new int[2], 0, new int[2], 1)) {
            Log.e(TAG, "Could not initialize egl");
        }

        int[] attribList = { //
                EGL14.EGL_RED_SIZE, 8, //
                EGL14.EGL_GREEN_SIZE, 8, //
                EGL14.EGL_BLUE_SIZE, 8, //
                EGL14.EGL_ALPHA_SIZE, 8, //
                EGL14.EGL_RENDERABLE_TYPE, EGL_OPENGL_ES3_BIT_KHR, //
                EGL14.EGL_NONE //
        };

        EGLConfig[] configs = new EGLConfig[1];
        int[] numConfigs = new int[1];

        Log.d(TAG, "Choosing EGL config");
        if (!EGL14.eglChooseConfig(eglDisplay, attribList, 0, configs, 0, configs.length, numConfigs, 0)) {
            captureLock.unlock();
            throw new RuntimeException("Failed to chose config");
        }

        EGLConfig config = configs[0];

        int[] attrib3_list = {EGL14.EGL_CONTEXT_CLIENT_VERSION, 3, EGL14.EGL_NONE};

        try {
            Log.d(TAG, "Creating EGL context");
            eglContext = EGL14.eglCreateContext(eglDisplay, config, EGL14.EGL_NO_CONTEXT, attrib3_list, 0);
        } catch (Throwable e) {
            Log.e(TAG, "Failed to create egl context: " + e);
        }

        DisplayMetrics metrics = getResources().getDisplayMetrics();
        int srcWidth = metrics.widthPixels;
        int srcHeight = metrics.heightPixels;
        int srcDensity = metrics.densityDpi;

        srcDims = Objects.requireNonNullElseGet(suggestedDims, () -> new Dimensions(srcWidth, srcHeight));
        downscaledDims = srcDims.scale(maxDims);
        uvDims = new Dimensions(downscaledDims.width / 2, downscaledDims.height / 2);

        int[] surfaceAttribs = {EGL14.EGL_WIDTH, downscaledDims.width, EGL14.EGL_HEIGHT, downscaledDims.height, EGL14.EGL_NONE};
        Log.d(TAG, "Creating EGL surface");
        eglSurface = EGL14.eglCreatePbufferSurface(eglDisplay, configs[0], surfaceAttribs, 0);
        if (eglSurface == EGL14.EGL_NO_SURFACE) {
            Log.e(TAG, "EGL create surface failed: " + EGL14.eglGetError());
            // TODO: return
        }

        Log.d(TAG, "Making EGL current");
        if (!EGL14.eglMakeCurrent(eglDisplay, eglSurface, eglSurface, eglContext)) {
            Log.e(TAG, "EGL make current failed: " + EGL14.eglGetError());
            // TODO: return
        }

        yFramebuffer = new Framebuffer(downscaledDims);
        megabuffer = new Megabuffer(downscaledDims);

        yProg = new Program(nativeGetVertShader(), nativeGetYFragShader());
        megaProg = new MegaProgram(nativeGetVertShader(), nativeGetUVFragShader());

        int[] vbos = new int[1];
        glGenBuffers(1, vbos, 0);
        vboId = vbos[0];

        glBindBuffer(GL_ARRAY_BUFFER, vboId);
        int float_size = 4;
        FloatBuffer vertexBuffer = ByteBuffer.allocateDirect(quad.length * float_size).order(ByteOrder.nativeOrder()).asFloatBuffer();
        vertexBuffer.put(quad);
        vertexBuffer.position(0);
        glBufferData(GL_ARRAY_BUFFER, quad.length * float_size, vertexBuffer, GL_STATIC_DRAW);
        glBindBuffer(GL_ARRAY_BUFFER, 0);

        nativeEglCtx = nativeSetupEgl();
        oesTexId = eglGetOesTexId(nativeEglCtx);

        surfaceTexture = new SurfaceTexture(oesTexId);
        surfaceTexture.setDefaultBufferSize(srcDims.width, srcDims.height);
        surfaceTexture.setOnFrameAvailableListener(surfaceTexture -> {
            if (!shouldCapture.get()) {
                return;
            }

            synchronized (captureLock) {
                try {
                    onFrameAvailable(surfaceTexture);
                } catch (RuntimeException e) {
                    Log.e(TAG, "onFrameAvailable failed: " + e);
                }
            }
        }, glHandler);

        surface = new Surface(surfaceTexture);

        if (virtualDisplay == null) {
            virtualDisplay = mediaProjection.createVirtualDisplay("ScreenCapture", srcDims.width, srcDims.height, srcDensity, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR | DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC | DisplayManager.VIRTUAL_DISPLAY_FLAG_PRESENTATION, surface, null, null);
        } else {
            Log.d(TAG, "Reusing virtual display");
            virtualDisplay.setSurface(surface);
            virtualDisplay.resize(srcDims.width, srcDims.height, srcDensity);
        }

        // EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);

        shouldCapture.set(true);

        captureLock.unlock();
    }

    // Called from native code
    private void startScreenCapture(int scaleWidth, int scaleHeight, int maxFramerate) {
        Log.d(TAG, "Requesting screen capture permissions");
        userMaxWidth = scaleWidth;
        userMaxHeight = scaleHeight;
        userMaxFps = maxFramerate;
        MediaProjectionManager projectionManager = (MediaProjectionManager) getSystemService(Context.MEDIA_PROJECTION_SERVICE);
        startActivityForResult(projectionManager.createScreenCaptureIntent(), REQUEST_CODE);
    }

    private void cleanupCapture(boolean shouldEmitStopSignal) {
        if (!shouldCapture.get()) {
            // Already stopped
            return;
        }

        Log.d(TAG, "Stopping capture");

        shouldCapture.set(false);

        glHandler.post(() -> {
            synchronized (captureLock) {
                glDeleteProgram(yProg.program);
                glDeleteProgram(megaProg.program);
                yProg = null;
                megaProg = null;
                glDeleteFramebuffers(2, new int[]{yFramebuffer.fboId, megabuffer.fboId}, 0);
                glDeleteTextures(3, new int[]{yFramebuffer.texId, megabuffer.uTexId, megabuffer.vTexId}, 0);
                yFramebuffer = null;
                megabuffer = null;

                nativeTeardownEgl(nativeEglCtx);
                nativeEglCtx = 0;

                EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);
                EGL14.eglDestroySurface(eglDisplay, eglSurface);
                eglSurface = EGL14.EGL_NO_SURFACE;
                EGL14.eglDestroyContext(eglDisplay, eglContext);
                eglContext = EGL14.EGL_NO_CONTEXT;
                eglDisplay = EGL14.EGL_NO_DISPLAY;

                if (shouldEmitStopSignal && virtualDisplay != null) {
                    virtualDisplay.release();
                    virtualDisplay = null;
                    Log.d(TAG, "Virtual display released");
                }
                if (shouldEmitStopSignal && mediaProjection != null) {
                    mediaProjection.stop();
                    mediaProjection = null;
                    Log.d(TAG, "Media projection stopped");
                }

                if (surfaceTexture != null) {
                    surfaceTexture.release();
                    surfaceTexture = null;
                    Log.d(TAG, "Surface texture released");
                }
                if (surface != null) {
                    surface.release();
                    surface = null;
                }

                if (shouldEmitStopSignal) {
                    nativeCaptureStopped();
                }
            }
        });
    }

    // Called from native code
    private void stopCapture() {
        cleanupCapture(true);
    }

    // Called from native code
    private void scanQr() {
        ScanOptions options = new ScanOptions();
        options.setDesiredBarcodeFormats(ScanOptions.QR_CODE);
        // NOTE: crashes if scan succeeds and the screen is oriented differently than what it was when the scan activity was started...
        options.setOrientationLocked(true);
        Intent intent = options.createScanIntent(this);
        startActivityForResult(intent, QR_SCAN_REQUEST_CODE);
    }

    private void initializeCapture(int resultCode, Intent data) {
        mediaProjection = mediaProjectionManager.getMediaProjection(resultCode, data);
        mediaProjection.registerCallback(projectionCallback, null);
        glHandler.post(() -> setupGles(new Dimensions(userMaxWidth, userMaxHeight), null));
        nativeCaptureStarted();
    }

    @Override
    protected void onActivityResult(int requestCode, int resultCode, Intent data) {
        super.onActivityResult(requestCode, resultCode, data);
        if (requestCode == REQUEST_CODE && resultCode == RESULT_OK) {
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
                Intent serviceIntent = new Intent(this, ScreenCaptureService.class);
                serviceIntent.setAction(ACTION_RESULT);
                serviceIntent.putExtra("resultCode", resultCode);
                serviceIntent.putExtra("data", data);

                Log.d(TAG, "Starting foreground service SDK=" + Build.VERSION.SDK_INT);

                try {
                    startForegroundService(serviceIntent);
                } catch (Exception e) {
                    Log.e(TAG, "Failed to start foreground service: " + e);
                }
            } else {
                Log.d(TAG, "Starting capture");
                initializeCapture(resultCode, data);
            }
        } else if (requestCode == REQUEST_CODE && resultCode == RESULT_CANCELED) {
            Log.d(TAG, "Media projection Canceled");
            nativeCaptureCancelled();
        } else if (requestCode == QR_SCAN_REQUEST_CODE && resultCode == RESULT_OK) {
            String result = data.getStringExtra("SCAN_RESULT");
            nativeQrScanResult(result);
        }
    }

    native long nativeSetupEgl();
    native int eglGetOesTexId(long eglCtx);
    native void nativeProcessFrame(long eglCtx, int width, int height, int fps, int fbY, int fbUv);
    native void nativeTeardownEgl(long eglCtx);
    native void nativeCaptureStarted();
    native void nativeCaptureStopped();
    native void nativeCaptureCancelled();
    native void nativeQrScanResult(String result);
    native String nativeGetVertShader();
    native String nativeGetYFragShader();
    native String nativeGetUVFragShader();

    static class MegaProgram {
        public int program;
        public int position;
        public int texCoord;
        public int texMatrix;
        public int textureUniform;
        public int srcSize;

        MegaProgram(String vert, String frag) {
            program = createProgram(vert, frag);
            position = glGetAttribLocation(program, "position");
            texCoord = glGetAttribLocation(program, "in_tex_coord");
            texMatrix = glGetUniformLocation(program, "u_tex_matrix");
            textureUniform = glGetUniformLocation(program, "u_texture");
            srcSize = glGetUniformLocation(program, "u_src_size");
        }

        private int loadShader(int shaderType, String source) {
            int shader = glCreateShader(shaderType);
            if (shader != 0) {
                glShaderSource(shader, source);
                glCompileShader(shader);

                int[] compiled = new int[1];
                glGetShaderiv(shader, GL_COMPILE_STATUS, compiled, 0);
                if (compiled[0] == 0) {
                    Log.e(TAG, "Could not compile shader " + shaderType + ": " + glGetShaderInfoLog(shader));
                    glDeleteShader(shader);
                    shader = 0;
                }
            }
            return shader;
        }

        private int createProgram(String vertSource, String fragmentSource) {
            int vert = loadShader(GL_VERTEX_SHADER, vertSource);
            if (vert == 0) {
                return 0;
            }

            int frag = loadShader(GL_FRAGMENT_SHADER, fragmentSource);
            if (frag == 0) {
                glDeleteShader(vert);
                return 0;
            }

            int program = glCreateProgram();
            if (program != 0) {
                glAttachShader(program, vert);
                glAttachShader(program, frag);
                glLinkProgram(program);

                int[] linkStatus = new int[1];
                glGetProgramiv(program, GL_LINK_STATUS, linkStatus, 0);
                if (linkStatus[0] != GL_TRUE) {
                    Log.e(TAG, "Could not link program: " + glGetProgramInfoLog(program));
                    glDeleteProgram(program);
                    program = 0;
                }
            }

            glDeleteShader(vert);
            glDeleteShader(frag);

            return program;
        }
    }

    static class Program {
        public int program;
        public int position;
        public int texCoord;
        public int texMatrix;
        public int textureUniform;

        // Must be called from a thread with a valid gl context
        Program(String vert, String frag) {
            program = createProgram(vert, frag);
            position = glGetAttribLocation(program, "position");
            texCoord = glGetAttribLocation(program, "in_tex_coord");
            texMatrix = glGetUniformLocation(program, "u_tex_matrix");
            textureUniform = glGetUniformLocation(program, "u_texture");
        }

        private int loadShader(int shaderType, String source) {
            int shader = glCreateShader(shaderType);
            if (shader != 0) {
                glShaderSource(shader, source);
                glCompileShader(shader);

                int[] compiled = new int[1];
                glGetShaderiv(shader, GL_COMPILE_STATUS, compiled, 0);
                if (compiled[0] == 0) {
                    Log.e(TAG, "Could not compile shader " + shaderType + ": " + glGetShaderInfoLog(shader));
                    glDeleteShader(shader);
                    shader = 0;
                }
            }
            return shader;
        }

        private int createProgram(String vertSource, String fragmentSource) {
            int vert = loadShader(GL_VERTEX_SHADER, vertSource);
            if (vert == 0) {
                return 0;
            }

            int frag = loadShader(GL_FRAGMENT_SHADER, fragmentSource);
            if (frag == 0) {
                glDeleteShader(vert);
                return 0;
            }

            int program = glCreateProgram();
            if (program != 0) {
                glAttachShader(program, vert);
                glAttachShader(program, frag);
                glLinkProgram(program);

                int[] linkStatus = new int[1];
                glGetProgramiv(program, GL_LINK_STATUS, linkStatus, 0);
                if (linkStatus[0] != GL_TRUE) {
                    Log.e(TAG, "Could not link program: " + glGetProgramInfoLog(program));
                    glDeleteProgram(program);
                    program = 0;
                }
            }

            glDeleteShader(vert);
            glDeleteShader(frag);

            return program;
        }
    }

    static void setupTexture2D(int texId, int width, int height) {
        glBindTexture(GL_TEXTURE_2D, texId);
        glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, width, height, 0, GL_RED, GL_UNSIGNED_BYTE, null);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    }

    static class Megabuffer {
        int fboId;
        int uTexId;
        int vTexId;
        Dimensions dims;

        Megabuffer(Dimensions dims) throws RuntimeException {
            this.dims = dims;

            int[] fbos = new int[1];
            int[] texs = new int[2];

            glGenFramebuffers(1, fbos, 0);
            glGenTextures(2, texs, 0);
            fboId = fbos[0];
            uTexId = texs[0];
            vTexId = texs[1];

            glBindFramebuffer(GL_FRAMEBUFFER, fboId);

            setupTexture2D(uTexId, dims.width / 2, dims.height / 2);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, uTexId, 0);

            setupTexture2D(vTexId, dims.width / 2, dims.height / 2);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT1, GL_TEXTURE_2D, vTexId, 0);

            int status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            if (status != GL_FRAMEBUFFER_COMPLETE) {
                throw new RuntimeException("FBO setup failed: " + status);
            }
        }
    }

    static class Framebuffer {
        int fboId;
        int texId;
        Dimensions dims;

        Framebuffer(Dimensions dims) throws RuntimeException {
            this.dims = dims;

            int[] fbos = new int[1];
            int[] texs = new int[1];

            glGenFramebuffers(1, fbos, 0);
            glGenTextures(1, texs, 0);
            fboId = fbos[0];
            texId = texs[0];

            setupTexture2D(texId, dims.width, dims.height);
            glBindFramebuffer(GL_FRAMEBUFFER, fboId);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texId, 0);

            int status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            if (status != GL_FRAMEBUFFER_COMPLETE) {
                throw new RuntimeException("FBO setup failed: " + status);
            }

            glBindFramebuffer(GL_FRAMEBUFFER, 0);
        }
    }

    public class CaptureBroadcastReceiver extends BroadcastReceiver {
        @Override
        public void onReceive(Context context, Intent intent) {
            Log.d(TAG, "Broadcast event intent=" + intent);

            if (ACTION_MEDIA_PROJECTION_STARTED.equals(intent.getAction())) {
                int resultCode = intent.getIntExtra("resultCode", Activity.RESULT_CANCELED);
                Intent data = intent.getParcelableExtra("data");
                initializeCapture(resultCode, data);
            }
        }
    }

    public class ProjectionCallback extends MediaProjection.Callback {
        @Override
        public void onStop() {
            stopCapture();
        }

        @RequiresApi(api = Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
        @Override
        public void onCapturedContentResize(int width, int height) {
            if (width == srcDims.width && height == srcDims.height) {
                // No change
                return;
            }

            Dimensions newDims = new Dimensions(width, height);
            if (shouldCapture.get() && virtualDisplay != null) {
                cleanupCapture(false);
                glHandler.post(() -> setupGles(new Dimensions(userMaxWidth, userMaxHeight), newDims));
            }
        }
    }
}
