use std::{sync::Arc, time::Duration};

use anyhow::Result;
use gst_gl::{GLDisplay, prelude::*};
use parking_lot::{Condvar, Mutex};
use tracing::{debug, error, instrument};

#[derive(Clone)]
pub struct GlContext {
    pub contexts: Arc<Mutex<Option<(GLDisplay, gst_gl::GLContext)>>>,
    cvar: Arc<Condvar>,
}

impl GlContext {
    pub fn new() -> Self {
        Self {
            contexts: Default::default(),
            cvar: Arc::new(Condvar::new()),
        }
    }

    pub fn set_contexts(&self, display: GLDisplay, context: gst_gl::GLContext) {
        *self.contexts.lock() = Some((display, context));
        self.cvar.notify_all();
    }

    pub fn deactivate_and_clear(&self) {
        if let Some((_display, gl_context)) = self.contexts.lock().take() {
            let _ = gl_context.activate(false);
        }
    }

    /// Tries for up to `timeout` to wait for the contexts to become available.
    ///
    /// Returns wheter the contexts are available or not.
    pub fn try_wait_available(&self, timeout: Duration) -> bool {
        let mut contexts = self.contexts.lock();
        if contexts.is_some() {
            return true;
        }
        self.cvar.wait_for(&mut contexts, timeout);
        contexts.is_some()
    }

    #[instrument(skip_all)]
    pub fn handle_need_context_msg(&self, typ: &str, element: &gst::Element) {
        if typ != *gst_gl::GL_DISPLAY_CONTEXT_TYPE && typ != "gst.gl.app_context" {
            return;
        }

        let max_retries = 3;
        for i in 0..max_retries {
            let mut contexts = self.contexts.lock();
            match contexts.as_ref() {
                Some((display, context)) => {
                    debug!(typ, "Providing context");
                    if typ == *gst_gl::GL_DISPLAY_CONTEXT_TYPE {
                        let display_ctx = gst::Context::new(typ, true);
                        display_ctx.set_gl_display(display);
                        element.set_context(&display_ctx);
                    } else if typ == "gst.gl.app_context" {
                        let mut app_ctx = gst::Context::new(typ, true);
                        let structure = app_ctx.get_mut().unwrap().structure_mut();
                        debug!(app_context_display_type = ?context.display().handle_type());
                        structure.set("context", context);
                        element.set_context(&app_ctx);
                    }
                }
                None if i < max_retries - 1 => {
                    debug!("Context not available yet, waiting");
                    self.cvar
                        .wait_for(&mut contexts, Duration::from_millis(300));
                }
                _ => error!("No context available, maximum wait time reached"),
            }
        }
    }
}

#[allow(clippy::large_enum_variant)]
pub enum GraphicsContext {
    None,
    // #[cfg(target_os = "linux")]
    // Egl(glutin_egl_sys::egl::Egl),
    // #[cfg(target_os = "linux")]
    // Glx(glutin_glx_sys::glx::Glx),
    #[cfg(target_os = "windows")]
    Wgl(u32),
    #[cfg(target_os = "macos")]
    Cgl,
    Initialized,
}

impl GraphicsContext {
    // #[cfg(any(target_os = "linux", target_os = "android"))]
    // fn is_on_wayland() -> Result<bool> {
    //     if std::env::var("WAYLAND_DISPLAY").is_ok() {
    //         Ok(true)
    //     } else if std::env::var("DISPLAY").is_ok() {
    //         Ok(false)
    //     } else {
    //         anyhow::bail!("Unsupported platform")
    //     }
    // }

    #[allow(unused)]
    pub fn from_slint(api: &slint::GraphicsAPI<'_>) -> Result<Self> {
        match api {
            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
                // #[cfg(any(target_os = "linux", target_os = "android"))]
                // match Self::is_on_wayland() {
                //     // NOTE: If error: assume KMS or Android
                //     Ok(true) | Err(_) => {
                //         Ok(Self::Egl(glutin_egl_sys::egl::Egl::load_with(|symbol| {
                //             get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                //         })))
                //     }
                //     Ok(false) => Ok(Self::Glx(glutin_glx_sys::glx::Glx::load_with(|symbol| {
                //         get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                //     }))),
                // }
                #[cfg(target_os = "windows")]
                {
                    use glow::HasContext;
                    let gl = unsafe {
                        glow::Context::from_loader_function_cstr(|s| get_proc_address(s))
                    };
                    tracing::debug!(wgl_version = ?gl.version());
                    return Ok(Self::Wgl(gl.version().major));
                }
                #[cfg(target_os = "macos")]
                return Ok(Self::Cgl);
            }
            _ => panic!("Unsupported graphics API"),
        }
    }

    #[instrument(skip_all)]
    pub fn get_gst_contexts(&self) -> Option<(gst_gl::GLContext, GLDisplay)> {
        match &self {
            // #[cfg(target_os = "linux")]
            // GraphicsContext::Egl(egl) => {
            //     let platform = gst_gl::GLPlatform::EGL;

            //     unsafe {
            //         let egl_display = egl.GetCurrentDisplay();
            //         let display =
            //             gst_gl_egl::GLDisplayEGL::with_egl_display(egl_display as usize).unwrap();
            //         let native_context = egl.GetCurrentContext();

            //         Some((
            //             gst_gl::GLContext::new_wrapped(
            //                 &display,
            //                 native_context as _,
            //                 platform,
            //                 gst_gl::GLContext::current_gl_api(platform).0,
            //             )
            //             .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))
            //             .unwrap(),
            //             display.upcast(),
            //         ))
            //     }
            // }
            // #[cfg(target_os = "linux")]
            // GraphicsContext::Glx(glx) => {
            //     let platform = gst_gl::GLPlatform::GLX;

            //     unsafe {
            //         let glx_display = glx.GetCurrentDisplay();
            //         let display =
            //             gst_gl_x11::GLDisplayX11::with_display(glx_display as usize).unwrap();
            //         let native_context = glx.GetCurrentContext();

            //         Some((
            //             gst_gl::GLContext::new_wrapped(
            //                 &display,
            //                 native_context as _,
            //                 platform,
            //                 gst_gl::GLContext::current_gl_api(platform).0,
            //             )
            //             .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))
            //             .unwrap(),
            //             display.upcast(),
            //         ))
            //     }
            // }
            #[cfg(target_os = "windows")]
            GraphicsContext::Wgl(major_version) => {
                let platform = gst_gl::GLPlatform::WGL;
                let gl_api = if *major_version >= 3 {
                    gst_gl::GLAPI::OPENGL3
                } else {
                    gst_gl::GLAPI::OPENGL
                };
                let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

                if gl_ctx == 0 {
                    panic!("Failed to create GL context");
                }

                let Some(gst_display) = GLDisplay::with_type(gst_gl::GLDisplayType::WIN32) else {
                    panic!("Failed to create GLDisplay of type WIN32");
                };

                gst_display.filter_gl_api(gl_api);

                unsafe {
                    Some((
                        gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api)
                            .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))
                            .unwrap(),
                        gst_display,
                    ))
                }
            }
            #[cfg(target_os = "macos")]
            GraphicsContext::Cgl => {
                let platform = gst_gl::GLPlatform::CGL;
                let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
                let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

                if gl_ctx == 0 {
                    panic!("Failed to get handle from CGL");
                }

                let gst_display = GLDisplay::new();
                unsafe {
                    let wrapped_context =
                        gst_gl::GLContext::new_wrapped(&gst_display, gl_ctx, platform, gl_api);

                    let wrapped_context = match wrapped_context {
                        None => {
                            panic!("Failed to create wrapped GL context");
                        }
                        Some(wrapped_context) => wrapped_context,
                    };

                    Some((wrapped_context, gst_display))
                }
            }
            _ => None,
        }
    }
}
