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

fn with_activity(what: &str, f: impl FnOnce(&mut jni::JNIEnv, &jni::objects::JObject) -> jni::errors::Result<()>) {
    let activity_ptr = ACTIVITY.load(Ordering::Acquire);
    if activity_ptr == 0 {
        tracing::warn!(what, "activity call before the activity was captured");
        return;
    }
    let ctx = ndk_context::android_context();
    let Ok(vm) = (unsafe { jni::JavaVM::from_raw(ctx.vm().cast()) }) else {
        return;
    };
    let Ok(mut env) = vm.attach_current_thread() else {
        return;
    };
    let activity = unsafe { jni::objects::JObject::from_raw(activity_ptr as jni::sys::jobject) };
    if let Err(err) = f(&mut env, &activity) {
        let _ = env.exception_clear();
        tracing::warn!(?err, what, "activity call failed");
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
