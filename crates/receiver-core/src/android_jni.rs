//! The core's way into Java: static methods on ReceiverCore, which outlives
//! any activity. Set once by `nativeCoreInit` on a Java thread, whose class
//! loader sees the app's classes (FindClass on a native thread does not).

use std::sync::OnceLock;

use jni::{
    JNIEnv, JavaVM,
    objects::{GlobalRef, JClass, JValue},
};
use tracing::warn;

struct Java {
    vm: JavaVM,
    core: GlobalRef,
}

static JAVA: OnceLock<Java> = OnceLock::new();

/// From `ReceiverCore.nativeCoreInit`, with that class.
pub fn init(env: &mut JNIEnv, core: &JClass) -> jni::errors::Result<()> {
    let java = Java {
        vm: env.get_java_vm()?,
        core: env.new_global_ref(core)?,
    };
    let _ = JAVA.set(java);
    Ok(())
}

/// Runs `f` against the ReceiverCore class in a local frame: the callers sit
/// on permanently attached threads, where locals are never freed otherwise.
pub fn with_core<R>(
    what: &str,
    f: impl FnOnce(&mut JNIEnv, &JClass) -> jni::errors::Result<R>,
) -> Option<R> {
    let Some(java) = JAVA.get() else {
        warn!(what, "ReceiverCore call before nativeCoreInit");
        return None;
    };
    let mut env = java.vm.attach_current_thread_permanently().ok()?;
    // a view of the global ref, nothing to delete
    let core = unsafe { JClass::from_raw(java.core.as_obj().as_raw()) };
    match env.with_local_frame(8, |env| f(env, &core)) {
        Ok(v) => Some(v),
        Err(err) => {
            let _ = env.exception_clear();
            warn!(?err, what, "ReceiverCore call failed");
            None
        }
    }
}

/// A void static method on ReceiverCore. False when it could not be called.
pub fn call(method: &str, sig: &str, args: &[JValue<'_, '_>]) -> bool {
    with_core(method, |env, core| {
        env.call_static_method(core, method, sig, args).map(drop)
    })
    .is_some()
}

/// Whether the process outlives a destroyed activity (the service runs).
pub fn keep_alive() -> bool {
    with_core("keepAlive", |env, core| {
        env.call_static_method(core, "keepAlive", "()Z", &[])?.z()
    })
    .unwrap_or(false)
}

