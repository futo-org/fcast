use anyhow::Result;
use gst::prelude::*;

pub enum GraphicsContext {
    None,
    #[cfg(target_os = "linux")]
    Egl(glutin_egl_sys::egl::Egl),
    #[cfg(target_os = "linux")]
    Glx(glutin_glx_sys::glx::Glx),
    #[cfg(target_os = "windows")]
    Wgl,
    #[cfg(target_os = "macos")]
    Cgl,
    Initialized,
}

impl GraphicsContext {
    #[cfg(target_os = "linux")]
    fn is_on_wayland() -> Result<bool> {
        if std::env::var("WAYLAND_DISPLAY").is_ok() {
            Ok(true)
        } else if std::env::var("DISPLAY").is_ok() {
            Ok(false)
        } else {
            anyhow::bail!("Unsupported platform")
        }
    }

    #[cfg(target_os = "linux")]
    fn get_egl_ctx(api: &slint::GraphicsAPI<'_>) -> Result<glutin_egl_sys::egl::Egl> {
        Ok(match api {
            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
                glutin_egl_sys::egl::Egl::load_with(|symbol| {
                    get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                })
            }
            _ => anyhow::bail!("Unsupported graphics API"),
        })
    }

    #[cfg(target_os = "linux")]
    fn get_glx_ctx(api: &slint::GraphicsAPI<'_>) -> Result<glutin_glx_sys::glx::Glx> {
        Ok(match api {
            slint::GraphicsAPI::NativeOpenGL { get_proc_address } => {
                glutin_glx_sys::glx::Glx::load_with(|symbol| {
                    get_proc_address(&std::ffi::CString::new(symbol).unwrap())
                })
            }
            _ => anyhow::bail!("Unsupported graphics API"),
        })
    }

    #[allow(unused)]
    pub fn from_slint(api: &slint::GraphicsAPI<'_>) -> Result<Self> {
        #[cfg(target_os = "linux")]
        match Self::is_on_wayland() {
            // NOTE: If error: assume KMS
            Ok(true) | Err(_) => {
                return Ok(Self::Egl(Self::get_egl_ctx(api)?));
            }
            Ok(false) => return Ok(Self::Glx(Self::get_glx_ctx(api)?)),
        }
        #[cfg(target_os = "android")]
        return Ok(Self::Egl(Self::get_egl_ctx(api)?));
        #[cfg(target_os = "windows")]
        return Ok(Self::Wgl);
        #[cfg(target_os = "macos")]
        return Ok(Self::Cgl);
    }

    pub fn get_gst_contexts(&self) -> Option<(gst_gl::GLContext, gst_gl::GLDisplay)> {
        match &self {
            #[cfg(target_os = "linux")]
            GraphicsContext::Egl(egl) => {
                let platform = gst_gl::GLPlatform::EGL;

                unsafe {
                    let egl_display = egl.GetCurrentDisplay();
                    let display =
                        gst_gl_egl::GLDisplayEGL::with_egl_display(egl_display as usize).unwrap();
                    let native_context = egl.GetCurrentContext();

                    Some((
                        gst_gl::GLContext::new_wrapped(
                            &display,
                            native_context as _,
                            platform,
                            gst_gl::GLContext::current_gl_api(platform).0,
                        )
                        .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))
                        .unwrap(),
                        display.upcast(),
                    ))
                }
            }
            #[cfg(target_os = "linux")]
            GraphicsContext::Glx(glx) => {
                let platform = gst_gl::GLPlatform::GLX;

                unsafe {
                    let glx_display = glx.GetCurrentDisplay();
                    let display =
                        gst_gl_x11::GLDisplayX11::with_display(glx_display as usize).unwrap();
                    let native_context = glx.GetCurrentContext();

                    Some((
                        gst_gl::GLContext::new_wrapped(
                            &display,
                            native_context as _,
                            platform,
                            gst_gl::GLContext::current_gl_api(platform).0,
                        )
                        .ok_or(anyhow::anyhow!("unable to create wrapped GL context"))
                        .unwrap(),
                        display.upcast(),
                    ))
                }
            }
            #[cfg(target_os = "windows")]
            GraphicsContext::Wgl => {
                let platform = gst_gl::GLPlatform::WGL;
                let gl_api = gst_gl::GLAPI::OPENGL3;
                let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

                if gl_ctx == 0 {
                    bail!("Failed to create GL context");
                }

                let Some(gst_display) = gst_gl::GLDisplay::with_type(gst_gl::GLDisplayType::WIN32)
                else {
                    bail!("Failed to create GLDisplay of type WIN32");
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
                debug!("Creating CGL context");

                let platform = gst_gl::GLPlatform::CGL;
                let (gl_api, _, _) = gst_gl::GLContext::current_gl_api(platform);
                let gl_ctx = gst_gl::GLContext::current_gl_context(platform);

                if gl_ctx == 0 {
                    panic!("Failed to get handle from CGL");
                }

                let gst_display = gst_gl::GLDisplay::new();
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
