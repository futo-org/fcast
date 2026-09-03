# Android receiver playback profiling (Galaxy S8, Exynos 8895, API 28)

STATUS 2026-09-01: all fixes below implemented, device-verified and
committed (fcast db95ae3d + decode-selection round earlier, flapjack
a097eb9, slint-forked 0d702d44d). User-confirmed: letterbox. Verified by
screenshot: PGS cue placement, one-line OSD title, OSD above nav bar.
Verified by measurement: h264 hw flip, vp9 p2 fallback, damper (101->2
frames/10s), avdec/dav1d caps, dataspace fallback (0 warnings), no
memory leak over 8 cycles. Device still runs the DEBUG apk for further
profiling; install release before day-to-day use. svc power stayon usb
is armed on the device for this session.

Session 2026-09-01. Muted casts over LAN, 10s steady-state windows, ticks are
USER_HZ (1 tick = 10ms CPU). "pct" = percent of one core. Range-capable http
server (harness note: python http.server has no Range support and silently
breaks start offsets AND moov-at-end mp4s; rangesrv.py in scratchpad).

## Baseline matrix (pre-fix build)

| case | decode path | total pct | notes |
|---|---|---|---|
| h264 1080p (YIFY mp4) | avdec SW (BUG 1) | 88 | should be hw |
| h264 4K24 (Oceans 13) | MediaCodec direct | 46 | reference good case |
| h264 4K60 (DJI moov-at-end) | SW, stalls (BUG 1) | dead | spinner forever |
| h264 640x360 (bbb m2ts) | avdec SW (BUG 1) | 63 | |
| hevc 1080p 8bit | MediaCodec direct | 59 | |
| hevc 1080p 10bit | MediaCodec direct | 57 | |
| hevc 4K HDR10 | MediaCodec direct | 29 | HDR metadata passed |
| vp9 4K24 (webm) | MediaCodec direct | 44 | |
| vp9 4K60 8bit | MediaCodec direct | ~50 | plays fine despite xml blocks/s limit |
| vp9 4K60 10bit HDR (profile 2) | MediaCodec dies (BUG 2) | dead | OMX 0x90000012 loop |
| vp9 360p profile 2 | MediaCodec dies (BUG 2) | dead | proves profile, not size |
| av1 1080p (dav1d SW, no hw) | dav1d 6 workers | 154-214 | plays; expected on this SoC |
| av1 4K HDR10+ (dav1d SW) | dav1d | 273 | PSS 686 MB (BUG 4 risk) |
| mpeg4 ASP 720p (XviD avi) | avdec SW | 77 | device claims mp4v-es ASP hw (deferred) |
| audio-only m4a | | 35 | 24 pct of it is UI redraw (BUG 3) |
| idle screen | | 0.6 | clean |

## Bugs found

1. h264 hw decode rejected for most real-world files. amcvideodec pins
   interlace-mode=progressive, but h264parse marks any stream whose VUI has
   pic_struct_present_flag as "mixed" (x264 sets it routinely; YIFY, DJI,
   bbb all progressive). Fix: gate on coded-picture-structure=frame
   (set from SPS frame_mbs_only_flag, the actual field-coding signal)
   instead of interlace-mode for h264. h265parse never writes
   interlace-mode, keep the pin there.

2. VP9 Profile 2 (10-bit): device advertises 0x1,0x4,0x1000 but
   OMX.Exynos.vp9.dec errors 0x90000012 on any profile-2 stream, even
   360p. Runtime decoder death = pipeline error, no re-autoplug. Fixes:
   probe profiles from HARDWARE decoders only (OMX.google/c2.android
   claims currently pollute the union) + distrust-quirk for Exynos vp9
   profile>=2 if the hw claim persists.

3. UI redraws at ~10Hz during playback with controls hidden (SurfaceFlinger:
   101 UI frames/10s, video on its own SurfaceView). android_main+mali =
   14-24 pct of a core in EVERY playing case incl. audio-only. No slint
   Timer/animation found at 100ms; cause under investigation via simpleperf
   (needs symbolized build).

4. 4K AV1 software decode peaks 686 MB PSS (dav1d frame threading).
   lmkd kill risk on 3-4 GB devices. Candidate: cap dav1ddec
   max-frame-delay/n-threads on android.

5. amcsurfacesink render_raw: ANativeWindow_setBuffersDataSpace returns
   -22 for every VIDEO dataspace on API 28 (NDK wrapper whitelists only
   sRGB-family until ~API 30). SW-lane colors default; HDR-via-dav1d shown
   without PQ declaration. Candidate: private perform(19) fallback
   (verify op constant) or accept and document.

6. media_codecs.xml blocks-per-second limits are NOT enforcement
   (vp9 4K60 8-bit exceeds the declared 979200 and plays). Do not encode
   rate limits into caps from them; size caps from VideoCapabilities are
   still worth adding for true-8K content.

## Startup latency (load timeline instrument, warm app)

- h264 4K: loaded 86ms, prerolled 590ms, playing 612ms
- hevc 1080p: 101 / 374 / 622ms; vp9 4K: 93 / 371 / 388ms
- hevc 4K HDR10: 184 / 538 / 823ms; vp9 4K60: 186 / 464 / 2297ms
- Cold-start only: first cast after app launch builds the codec in
  cpu-copy mode then rebuilds direct ~200ms later (surface handoff loses
  the race despite the LoadingMedia preopen). Self-heals; low priority.

## Fixes implemented (this round)

1. amcvideodec caps: h264 gates on coded-picture-structure=frame
   (interlace pin dropped for h264, kept for h265).
2. codeclist: probe skips software MediaCodec decoders (omx.google/
   c2.android name heuristic) for mimes AND profiles.
3. codeclist: pre-9000 samsungexynos VP9 quirk drops profile 2/3 claims.
4. codeclist + amcvideodec: VideoCapabilities max width/height per mime
   as template width/height ranges (shared bound both axes for portrait).
5. amcsurfacesink: setBuffersDataSpace private perform(19) fallback when
   the NDK wrapper EINVALs video dataspaces (API < ~33).
6. flapjack pipeline: dav1ddec max-frame-delay=2 on android (memory cap).
7. receiver-ui gui: TickDamper. Progress writes quantize to whole seconds
   and skip while the video OSD is hidden; buffered ranges push only on
   change. Kills the 10 fps parked-UI render loop.

## Post-fix verification (same device, helper procs now counted)

| case | decode | app pct | media.codec pct | PSS MB |
|---|---|---|---|---|
| h264 1080p | MediaCodec direct (was SW) | 38 (was 88) | 12 | 240 |
| h264 4K24 | MediaCodec direct | 36 | 14 | 373 |
| h264 4K30 moov-at-end | MediaCodec direct (was dead) | 4 (no audio track) | 2 | 192 |
| hevc10 1080p | MediaCodec direct | 45 | 11 | 248 |
| vp9 4K60 8bit | MediaCodec direct | 43 | 38 | 385 |
| vp9 profile2 360p | avdec SW, plays (was hw death) | 12 | 0 | 193 |
| vp9 profile2 4K60 | avdec SW slideshow (was hw death) | 594 | 0 | 773 |
| av1 1080p | dav1d delay=2 | 205 | 0 | 366 (was 392) |
| av1 4K HDR10+ | dav1d delay=2 | 226 | 0 | 568 (was 686) |
| h264 MBAFF (guard) | avdec SW, correct | 15 | 0 | 216 |
| audio m4a | | 22 (was 35) | 0 | 187 |

- UI frames during hidden-OSD playback: 101/10s -> 2/10s (damper).
- Audio view UI thread: 156 -> 37 ticks/10s.
- vp9 profile2 4K60 sw fallback saturates all 8 cores for a slideshow;
  avdec max-threads=4 cap added (post-measurement) to bound heat/memory.
  A "this device cannot play this" signal would be honest, needs a
  cheap hopelessness heuristic (size*fps vs sw budget). Deferred.
- media.codec process adds 10-40 pct of a core on hw decode (was
  invisible to per-app measurements); audioserver ~16-32 pct whenever
  audio plays, surfaceflinger 20-30 pct during any playback.

## Letterbox coordinate bug (user-reported, session 2026-09-01)

Video SurfaceView rect and cue overlays were computed against the slint
window size, which on this device includes window-surface insets (reports
1520x3040 for a 1440x2960 screen), and the view is margin-positioned in
android.R.id.content, a different origin when system bars are visible.
Fix: `get_content_width/height` on the Java video surface; letterbox and
cue geometry (android_subtitles) now share that space. Follow-up noted:
slint itself lays the OSD out in the padded space (potential ~40px
off-center chrome), a slint-fork window-size question. 

## Memory benchmarking (user request, debug build note: release is leaner)

- Steady-state PSS by case: audio 187, h264 1080p 240, hevc10 248,
  h264 4K 373, vp9 4K60 385, av1 1080p 366, av1 4K 10bit 568 (dav1d
  delay=2; was 686 at auto), vp9 sw-fallback 4K60 631 (was 773 pre
  thread cap). Device total RAM 3.7 GB, swap in heavy use system-wide.
- Leak check, 8 cast/stop cycles across h264-4K/av1/hevc10/audio:
  post-stop floor 197-224 MB, no upward trend (one 290 sample right
  after an av1 stop = teardown lag, next rounds back to ~200).
- media.codec PSS excluded (system-owned); codec_mem field records it.

## Black-screen wedge (user-reported "frozen", 2026-09-01 08:06)

One-shot race, reproduced once, recovered by casting again (no app
restart needed): stop + immersive-exit relayout + a same-size
NativeWindowResized landed right after the Idle repaint. The resized
window surface is reallocated (insets change the buffer geometry) but
slint saw no size change, dirtied nothing, and never repainted: black
screen with a fully healthy event loop (commands processed, taps eaten,
zero frames). Evidence: bugreport stacks (android_main idle in
ALooper_pollOnce), SurfaceFlinger 0 frames on tap, recovery on next
scene change. Fix: winit backend resize_event now request_redraw()s
unconditionally. Watch for recurrence.

## Bitmap-cue follow-ups (user-reported, Coneheads PGS; USER-CONFIRMED FIXED)

Two more insets bugs after the content-space round (fcast 40410456,
slint-fork 940a6433b): (1) slint draws overlays from the WINDOW origin
while the video view is margin-positioned in the content frame, so every
cue sat high/left by the system-bar offset whenever bars were visible;
cue rows now shift by the content frame's getLocationInWindow. Verified
against ground truth: PGS composition coords parsed straight out of the
SUP stream (1920x1080 authoring canvas, decoder fit+centres onto the
1938-coded picture correctly), rendered x matched authored x to ~4 px.
(2) already-shown cues never re-placed on fullscreen toggles (engine set
unchanged -> signature dedup skipped the rebuild); resync now clears the
signature and re-pushes.

## Verified non-issues

- HDR10/HLG passthrough on direct lane sets dataspace correctly (ACodec).
- hevc 10-bit, vp9 4K, h264 4K all direct-surface, 30-60 pct total.
- Idle app is quiet (0.6 pct).

## Harness

- profile.py <name> <path> <container> [start] [settle] [window] in scratchpad.
- rangesrv.py 8765 in ~/Videos (Range support).
- Muted via protocol volume 0; stop restores volume 1.0.
- svc power stayon usb armed on device for the session (restore at end).
- Debuggable build + /data/local/tmp/simpleperf for callgraphs.
- Test samples: prof-vp9p2.webm (profile2 360p), prof-h264-interlaced.mp4
  (MBAFF field-coded, must STAY software), both in ~/Videos.

## Time-to-first-frame round (2026-09-03, S8, release build)

Two field reports, one cause. Hiding the video SurfaceView by a 0x0 VISIBLE
layout does not destroy its surface on API 28 (creation seq stayed at 1
across every item): the previous item's last frame stayed in the buffer
queue and showed under the next item, and after any software-decoded item
(raw blit = ANativeWindow_lock, a CPU producer connection that never sheds)
every later MediaCodec configure failed with -10000, retried 60/250/600 ms
and fell back to cpu copy. Fix: INVISIBLE on hide (slint fork Java). That
exposed the cold-start race on every item: the fresh surface lands ~100 ms
after a fast load, the codec built cpu-copy, rebuilt direct, and dropped
frames until the next keyframe (4.94 s on the VP9 4K sample). Fix: window
promise (`set_video_window_pending`), the decoder waits up to 400 ms for a
promised surface before building headless.

Measured on 4K_sample_video.webm (VP9 3840x2160 29.97, opus): loaded 135 ms,
surface handed +10 ms, direct on first build, prerolled 453 ms, playing
463 ms. Before: 4.9 s to steady video.

## Profiling round (2026-09-03, S8, 4K VP9 30 fps over the v4 session)

release-prof library (thin LTO, unstripped) in the debuggable apk, simpleperf
dwarf call graphs, 10 s steady-state windows. pct = of one core.

| process | pass 1 | pass 2 |
|---|---|---|
| app | 48.4 | 45.6 |
| hwcomposer | 30.4 | 29.8 |
| surfaceflinger | 24.7 | 25.1 |
| audioserver (AudioOut_D) | 23.1 | 22.6 |
| media.codec | 18.2 | 19.7 |

App samples by thread: multiqueue1:src 26% (two threads: video input into
MediaCodec, and opusdec), NDK MediaCodec_ 17% + CodecLooper 14% (the codec
client loopers: binder, futex, cfi checks), tokio workers 23% (renamed
Thread-N by the unnamed JNI attach; TLS media ingest over the fcast session:
rustls AES-GCM 6% of them, recvfrom, memcpy), fj-vqueue 5%, multiqueue2 4%,
fcompsrc 4% (matroska demux + vp9 parser), fj-aqueue 2% (audiostretch
submit_input_buffer is a third of it), android_main 0.7%.

Video input chain: 59% of handle_frame_android is drain_outputs ->
AMediaCodec_dequeueOutputBuffer -> AMessage::postAndAwaitResponse (futex
signal + wait). The NDK sync API turns every codec call into a looper round
trip: ~4 per frame (dequeue_input, queue_input, dequeue_output x2 until
TRY_AGAIN) plus the release on the sink thread, ~150 round trips/s. Largest
candidate: AMediaCodec_setAsyncNotifyCallback (API 28) to drop the dequeue
polls, est. 10-15% of app samples. Top kernel address ffffff8008ce7c94 is
10.7% of all app samples (kptr hidden on the user build; context-switch
tail of the same messaging).

Opus decode ~12% of its thread: NEON intrinsics are on
(celt_pitch_xcorr_float_neon present), the FFT has no NEON path in libopus's
float build. No action. Audioserver 22% for one 48 kHz stream is the deep
buffer mixer plus vendor effects, unchanged from 09-01.

Tooling: `cargo ndk --target aarch64-linux-android -o <dir> build --package
receiver-android --profile release-prof` with the xtask env; AGP strips
jniLibs on packaging, so symbolize on the host by splicing the unstripped
.so into a mirror of the installed base.apk at its device path under
`simpleperf report --symfs` (the build had no build-id; rustflags now add
--build-id=sha1 so --symdir works next time). `setprop security.perf_harden
0`, record via `run-as <pkg> ./simpleperf record -p <pid> --call-graph
dwarf`, pull with `adb exec-out run-as <pkg> cat perf.data` (plain adb shell
corrupts binaries). Per-thread ticks from /proc/<pid>/task/*/stat deltas.
