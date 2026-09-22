//! Activity calls that only the activity can serve, over JNI: the immersive
//! toggle (system-ui visibility) and backgrounding the task. ndk-context only
//! carries the Application context; the ACTIVITY instance jobject comes
//! from `AndroidApp::activity_as_ptr` (a global ref android-activity keeps
//! alive for the app's life), captured once at startup.

use std::sync::atomic::{AtomicUsize, Ordering};

static ACTIVITY: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn init(app: &slint::android::AndroidApp) {
    ACTIVITY.store(app.activity_as_ptr() as usize, Ordering::Release);
}

fn with_activity(
    what: &str,
    f: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<()>,
) -> bool {
    let activity_ptr = ACTIVITY.load(Ordering::Acquire);
    if activity_ptr == 0 {
        tracing::warn!(what, "activity call before the activity was captured");
        return false;
    }
    let ctx = ndk_context::android_context();
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return false;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return false;
    };
    let activity = unsafe { jni::objects::JObject::from_raw(activity_ptr as jni::sys::jobject) };
    match f(&mut env, &activity) {
        Ok(()) => true,
        Err(err) => {
            let _ = env.exception_clear();
            tracing::warn!(?err, what, "activity call failed");
            false
        }
    }
}

pub fn set(on: bool) {
    with_activity("setImmersiveUi", |env, activity| {
        env.call_method(
            activity,
            "setImmersiveUi",
            "(Z)V",
            &[jni::objects::JValue::Bool(on as u8)],
        )
        .map(drop)
    });
}

/// Whether this device is a television (leanback). Read from UiModeManager
/// through the application context, so it works before the activity is
/// captured.
pub fn is_television() -> bool {
    let ctx = ndk_context::android_context();
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return false;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return false;
    };
    let context = unsafe { jni::objects::JObject::from_raw(ctx.context().cast()) };
    let result = (|| -> jni::errors::Result<bool> {
        let name = env.new_string("uimode")?;
        let manager = env
            .call_method(
                &context,
                "getSystemService",
                "(Ljava/lang/String;)Ljava/lang/Object;",
                &[jni::objects::JValue::Object(&name)],
            )?
            .l()?;
        if manager.is_null() {
            return Ok(false);
        }
        // Configuration.UI_MODE_TYPE_TELEVISION
        Ok(env.call_method(&manager, "getCurrentModeType", "()I", &[])?.i()? == 4)
    })();
    result.unwrap_or_else(|_| {
        let _ = env.exception_clear();
        false
    })
}

/// The current video aspect for the PiP window, deduped here so the
/// per-relayout call costs a JNI trip only when the shape changes.
pub fn set_video_aspect(w: u32, h: u32) {
    use std::sync::atomic::AtomicU64;
    static LAST: AtomicU64 = AtomicU64::new(0);
    let key = ((w as u64) << 32) | h as u64;
    if LAST.load(Ordering::Relaxed) == key {
        return;
    }
    let ok = with_activity("setVideoAspect", |env, activity| {
        env.call_method(
            activity,
            "setVideoAspect",
            "(II)V",
            &[
                jni::objects::JValue::Int(w as i32),
                jni::objects::JValue::Int(h as i32),
            ],
        )
        .map(drop)
    });
    if ok {
        // committed only on success, or one failed call would suppress
        // retries for this shape forever
        LAST.store(key, Ordering::Relaxed);
    }
}

/// Send the task to the back instead of finishing the activity: a finish
/// takes the whole process (and the cast) with it, see the entry crate's
/// exit-on-destroy.
pub fn move_task_to_back() {
    with_activity("moveTaskToBack", |env, activity| {
        env.call_method(
            activity,
            "moveTaskToBack",
            "(Z)Z",
            &[jni::objects::JValue::Bool(1)],
        )
        .map(drop)
    });
}
