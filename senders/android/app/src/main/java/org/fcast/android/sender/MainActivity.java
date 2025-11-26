package org.fcast.android.sender;

import static android.opengl.EGLExt.EGL_OPENGL_ES3_BIT_KHR;
import static android.opengl.GLES11Ext.GL_TEXTURE_EXTERNAL_OES;
import static android.opengl.GLES20.*;
import static android.opengl.GLES30.*;

import android.app.Activity;
import android.app.NativeActivity;
import android.content.BroadcastReceiver;
import android.content.Context;
import android.content.Intent;
import android.content.IntentFilter;
import android.content.res.*;
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
import android.os.*;
import android.util.DisplayMetrics;
import android.util.Log;
import android.view.*;

import androidx.annotation.NonNull;
import androidx.localbroadcastmanager.content.LocalBroadcastManager;

import com.journeyapps.barcodescanner.ScanOptions;

import org.freedesktop.gstreamer.GStreamer;

import java.net.Inet6Address;
import java.net.InetAddress;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.FloatBuffer;
import java.time.Duration;
import java.time.Instant;
import java.util.*;
import java.util.concurrent.atomic.*;
import java.util.concurrent.locks.*;
import java.util.stream.Collectors;

class FCastDiscoveryListener implements NsdManager.DiscoveryListener {
    private static final String TAG = "FCastDiscoveryListener";
    private final NsdManager nsdManager;


    FCastDiscoveryListener(NsdManager nsdManager) {
        this.nsdManager = nsdManager;
    }

    private static ByteBuffer addrConvert(InetAddress addr) {
        byte[] addrB = addr.getAddress();
        ByteBuffer buffer = ByteBuffer.allocateDirect(addrB.length);
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
        int x = width;
        int y = height;

        if (height > width) {
            int tmp = maxDims.height;
            maxDims.height = maxDims.width;
            maxDims.width = tmp;
        }

        float aspect_ratio = Math.min((float)maxDims.width / (float)x, (float)maxDims.height / (float)y);
        int scaledWidth = (int)((float)x * aspect_ratio);
        int scaledHeight = (int)((float)y * aspect_ratio);

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

    static {
        System.loadLibrary("gstreamer_android");
        System.loadLibrary("fcastsender");
    }

    private ProjectionCallback projectionCallback;
    private MediaProjectionManager mediaProjectionManager;
    private MediaProjection mediaProjection;
    private VirtualDisplay virtualDisplay;
    private SurfaceTexture surfaceTexture;
    private EGLContext eglContext = EGL14.EGL_NO_CONTEXT;
    private EGLDisplay eglDisplay = EGL14.EGL_NO_DISPLAY;
    private EGLSurface eglSurface = EGL14.EGL_NO_SURFACE;
    private Surface surface;
    private HandlerThread glThread;
    private Handler glHandler;
    private DisplayManager displayManager;
    private final ReentrantLock captureLock = new ReentrantLock();

    @Override
    public void onDisplayAdded(int displayId) { }

    @Override
    public void onDisplayRemoved(int displayId) { }

    @Override
    public void onConfigurationChanged(Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
    }

    @Override
    public void onDisplayChanged(int displayId) {
        if (this.getWindowManager().getDefaultDisplay().getDisplayId() != displayId) {
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
            glHandler.post(() -> setupGles(new Dimensions(1920, 1080), 30, newDims));
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

    private final CaptureBroadcastReceiver receiver = new CaptureBroadcastReceiver();

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

        glThread = new HandlerThread("OpenGLThread");
        glThread.start();
        glHandler = new Handler(glThread.getLooper());

        IntentFilter filter = new IntentFilter(ACTION_MEDIA_PROJECTION_STARTED);
        filter.addCategory(Intent.CATEGORY_DEFAULT);
        LocalBroadcastManager.getInstance(this).registerReceiver(receiver, filter);

        displayManager = (DisplayManager)getSystemService(Context.DISPLAY_SERVICE);
        displayManager.registerDisplayListener(this, new Handler(getMainLooper()));
    }

    private static final String vertexShader = """
            #extension GL_OES_EGL_image_external : require
            attribute vec4 aPosition;
            attribute vec4 aTexCoord;
            uniform mat4 uTexMatrix;

            varying vec2 vTexCoord;

            void main() {
                gl_Position = aPosition;
                vTexCoord = (uTexMatrix * aTexCoord).xy;
            }""";
    private static final String fragShaderHeader = """
            #extension GL_OES_EGL_image_external : require
            precision mediump float;

            varying vec2 vTexCoord;
            uniform samplerExternalOES sTexture;
            """;
    // Full range BT.709
    // Coefficients from https://en.wikipedia.org/wiki/YCbCr#ITU-R_BT.709_conversion
    private static final String fragmentShaderY = fragShaderHeader + """
            void main() {
                vec3 rgb = texture2D(sTexture, vTexCoord).rgb;
                float y = 0.2126 * rgb.r + 0.7152 * rgb.g + 0.0722 * rgb.b;

                gl_FragColor = vec4(y, 0.0, 0.0, 0.0);
            }""";
    // Inline 4:2:0 subsampling function where the sample is the average of all texels in the block
    private static final String subsampledRgb = """
                vec2 step = 1.0 / srcSize;
                vec3 rgbQ1 = texture2D(sTexture, vTexCoord).rgb;
                vec3 rgbQ2 = texture2D(sTexture, vTexCoord + vec2(step.x, 0.0)).rgb;
                vec3 rgbQ3 = texture2D(sTexture, vTexCoord + vec2(0.0, step.y)).rgb;
                vec3 rgbQ4 = texture2D(sTexture, vTexCoord + vec2(step.x, step.y)).rgb;

                // Compute average
                vec3 rgb = (rgbQ1 + rgbQ2 + rgbQ3 + rgbQ4) * 0.25;
            """;
    private static final String fragmentShaderU = fragShaderHeader + """
            uniform vec2 srcSize;

            void main() {""" + subsampledRgb + """
                // Add 0.5 to change the range from `-0.5 - 0.5` to `0 - 1`
                float u = -0.1146 * rgb.r - 0.3854 * rgb.g + 0.5 * rgb.b + 0.5;
                gl_FragColor = vec4(u, 0.0, 0.0, 0.0);
            }""";
    private static final String fragmentShaderV = fragShaderHeader + """
            uniform vec2 srcSize;

            void main() {""" + subsampledRgb + """
                float v = 0.5 * rgb.r - 0.4542 * rgb.g - 0.0458 * rgb.b + 0.5;
                gl_FragColor = vec4(v, 0.0, 0.0, 0.0);
            }""";
    Program yProg = null;
    Program uProg = null;
    Program vProg = null;
    Framebuffer yFramebuffer = null;
    Framebuffer uFramebuffer = null;
    Framebuffer vFramebuffer = null;
    private final float[] quad = { //
            -1f, -1f, 0f, 1f, //
            1f, -1f, 1f, 1f,  //
            -1f, 1f, 0f, 0f,  //
            1f, 1f, 1f, 0f,   //
    };
    int vboId;
    Dimensions srcDims = null;
    Dimensions downscaledDims = null;
    Dimensions uvDims = null;
    AtomicBoolean shouldCapture = new AtomicBoolean(false);
    int oesTexId;
    Instant lastFrameSent = Instant.EPOCH;
    int maxFps;

    private int createOesTexture() {
        int[] tex = new int[1];
        glGenTextures(1, tex, 0);

        glBindTexture(GL_TEXTURE_EXTERNAL_OES, tex[0]);

        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
        glTexParameteri(GL_TEXTURE_EXTERNAL_OES, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

        glBindTexture(GL_TEXTURE_EXTERNAL_OES, 0);

        return tex[0];
    }

    static class Program {
        public int program;
        public int position;
        public int texCoord;
        public int texMatrix;
        public int textureUniform;
        // Only present for chroma shaders
        public int srcSize = 0;

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

        private int createProgram(String fragmentSource) {
            int vert = loadShader(GL_VERTEX_SHADER, vertexShader);
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

        // Must be called from a thread with a valid gl context
        Program(String frag, boolean isChroma) {
            program = createProgram(frag);
            position = glGetAttribLocation(program, "aPosition");
            texCoord = glGetAttribLocation(program, "aTexCoord");
            texMatrix = glGetUniformLocation(program, "uTexMatrix");
            textureUniform = glGetUniformLocation(program, "sTexture");
            if (isChroma) {
                srcSize = glGetUniformLocation(program, "srcSize");
            }
        }
    }

    static class Framebuffer {
        int fboId;
        int texId;
        Dimensions dims;
        ByteBuffer buf = ByteBuffer.allocateDirect(1);

        Framebuffer(Dimensions dims) throws RuntimeException {
            this.dims = dims;

            int[] fbos = new int[1];
            int[] texs = new int[1];

            glGenFramebuffers(1, fbos, 0);
            glGenTextures(1, texs, 0);
            fboId = fbos[0];
            texId = texs[0];

            glBindTexture(GL_TEXTURE_2D, texId);

            glTexImage2D(GL_TEXTURE_2D, 0, GL_R8, dims.width, dims.height, 0, GL_RED, GL_UNSIGNED_BYTE, null);

            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
            glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);

            glBindFramebuffer(GL_FRAMEBUFFER, fboId);
            glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, texId, 0);

            int status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
            if (status != GL_FRAMEBUFFER_COMPLETE) {
                throw new RuntimeException("FBO setup failed: " + status);
            }

            glBindFramebuffer(GL_FRAMEBUFFER, 0);
        }

        private void readPixels() {
            glBindFramebuffer(GL_FRAMEBUFFER, fboId);
            if (buf.capacity() < dims.width * dims.height) {
                buf = ByteBuffer.allocateDirect(dims.width * dims.height);
            }
            buf.position(0);
            glReadPixels(0, 0, dims.width, dims.height, GL_RED, GL_UNSIGNED_BYTE, buf);
            glBindFramebuffer(GL_FRAMEBUFFER, 0);
        }
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

        if (prog.srcSize != 0) {
            glUniform2f(prog.srcSize, (float)srcDims.width, (float)srcDims.height);
        }

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
        if (!EGL14.eglMakeCurrent(eglDisplay, eglSurface, eglSurface, eglContext)) {
            throw new RuntimeException("EGL make current failed: " + EGL14.eglGetError());
        }

        surfaceTexture.updateTexImage();

        Instant now = Instant.now();
        // Drop early frames
        if (Duration.between(lastFrameSent, now).compareTo(Duration.ofMillis(1000 / maxFps)) < 0) {
            return;
        }

        float[] texMatrix = new float[16];
        surfaceTexture.getTransformMatrix(texMatrix);

        renderToFbWithProg(oesTexId, yFramebuffer, yProg, texMatrix);
        renderToFbWithProg(oesTexId, uFramebuffer, uProg, texMatrix);
        renderToFbWithProg(oesTexId, vFramebuffer, vProg, texMatrix);

        yFramebuffer.readPixels();
        uFramebuffer.readPixels();
        vFramebuffer.readPixels();

        EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);

        nativeProcessFrame(downscaledDims.width, downscaledDims.height, yFramebuffer.buf, uFramebuffer.buf, vFramebuffer.buf);

        lastFrameSent = now;
    }

    // TODO: handle errors
    private void setupGles(Dimensions maxDims, int maxFps, Dimensions suggestedDims) {
        captureLock.lock();

        this.maxFps = maxFps;

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

        oesTexId = createOesTexture();

        yFramebuffer = new Framebuffer(downscaledDims);
        uFramebuffer = new Framebuffer(uvDims);
        vFramebuffer = new Framebuffer(uvDims);

        yProg = new Program(fragmentShaderY, false);
        uProg = new Program(fragmentShaderU, true);
        vProg = new Program(fragmentShaderV, true);

        int[] vbos = new int[1];
        glGenBuffers(1, vbos, 0);
        vboId = vbos[0];

        glBindBuffer(GL_ARRAY_BUFFER, vboId);
        FloatBuffer vertexBuffer = ByteBuffer.allocateDirect(quad.length * 4).order(ByteOrder.nativeOrder()).asFloatBuffer();
        vertexBuffer.put(quad);
        vertexBuffer.position(0);
        glBufferData(GL_ARRAY_BUFFER, quad.length * 4, vertexBuffer, GL_STATIC_DRAW);
        glBindBuffer(GL_ARRAY_BUFFER, 0);

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

        virtualDisplay = mediaProjection.createVirtualDisplay("ScreenCapture", srcDims.width, srcDims.height, srcDensity, DisplayManager.VIRTUAL_DISPLAY_FLAG_AUTO_MIRROR | DisplayManager.VIRTUAL_DISPLAY_FLAG_PUBLIC | DisplayManager.VIRTUAL_DISPLAY_FLAG_PRESENTATION, surface, null, null);

        EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);

        shouldCapture.set(true);

        captureLock.unlock();
    }

    // Called from native code
    private void startScreenCapture() {
        Log.d(TAG, "Requesting screen capture permissions");
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
                if (!EGL14.eglMakeCurrent(eglDisplay, eglSurface, eglSurface, eglContext)) {
                    Log.e(TAG, "EGL make current failed: " + EGL14.eglGetError());
                    return;
                }

                glDeleteProgram(yProg.program);
                glDeleteProgram(uProg.program);
                glDeleteProgram(vProg.program);

                yProg = null;
                uProg = null;
                vProg = null;

                glDeleteFramebuffers(3, new int[]{yFramebuffer.fboId, uFramebuffer.fboId, vFramebuffer.fboId}, 0);

                glDeleteTextures(4, new int[]{oesTexId, yFramebuffer.texId, uFramebuffer.texId, vFramebuffer.texId}, 0);

                yFramebuffer = null;
                uFramebuffer = null;
                vFramebuffer = null;

                EGL14.eglMakeCurrent(eglDisplay, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_SURFACE, EGL14.EGL_NO_CONTEXT);

                EGL14.eglDestroySurface(eglDisplay, eglSurface);
                eglSurface = EGL14.EGL_NO_SURFACE;

                EGL14.eglDestroyContext(eglDisplay, eglContext);
                eglContext = EGL14.EGL_NO_CONTEXT;

                eglDisplay = EGL14.EGL_NO_DISPLAY;

                if (virtualDisplay != null) {
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
        glHandler.post(() -> setupGles(new Dimensions(1920, 1080), 30, null));
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

    native void nativeProcessFrame(int width, int height, ByteBuffer bufferY, ByteBuffer bufferU, ByteBuffer bufferV);

    native void nativeCaptureStarted();

    native void nativeCaptureStopped();

    native void nativeCaptureCancelled();

    native void nativeQrScanResult(String result);

    public class ProjectionCallback extends MediaProjection.Callback {
        @Override
        public void onStop() {
            stopCapture();
        }
    }
}
