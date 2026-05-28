pub struct Options {
    pub app_reverse_domain: String,
}

pub struct Inhibitor {
    imp: InhibitorImpl,
}

impl Inhibitor {
    pub fn new(options: Options) -> Self {
        Self {
            imp: InhibitorImpl::new(options),
        }
    }

    pub fn inhibit(&mut self, reason: &str) {
        self.imp.inhibit(reason);
    }

    pub fn un_inhibit(&mut self) {
        self.imp.un_inhibit();
    }
}

#[cfg(target_os = "windows")]
use windows::InhibitorImpl;
#[cfg(target_os = "windows")]
mod windows {
    use tracing::{error, instrument};
    use windows::core::Error as WindowsError;
    use windows::Win32::System::Power::{
        SetThreadExecutionState, ES_AWAYMODE_REQUIRED, ES_CONTINUOUS, ES_DISPLAY_REQUIRED,
        ES_SYSTEM_REQUIRED, EXECUTION_STATE,
    };

    use crate::Options;

    pub type Error = WindowsError;

    pub struct InhibitorImpl {
        #[allow(unused)]
        options: Options,
        previous: EXECUTION_STATE,
    }

    impl InhibitorImpl {
        pub fn new(options: Options) -> Self {
            InhibitorImpl {
                options,
                previous: Default::default(),
            }
        }

        #[instrument(skip_all)]
        fn inhibit(&mut self, _reason: &str)  {
            unsafe {
                self.previous = SetThreadExecutionState(ES_CONTINUOUS | ES_DISPLAY_REQUIRED);
                if self.previous == EXECUTION_STATE(0) {
                    error!(err = ?WindowsError::from_thread(), "Failed to set execution state");
                }
            }
        }

        #[instrument(skip_all)]
        pub fn un_inhibit(&mut self) {
            unsafe {
                SetThreadExecutionState(self.previous);
            }
        }
    }

    impl Drop for InhibitorImpl {
        fn drop(&mut self) {
            self.un_inhibit();
        }
    }
}

#[cfg(target_os = "macos")]
use macos::InhibitorImpl;
#[cfg(target_os = "macos")]
mod macos {
    use tracing::{error, instrument};
    use objc2_core_foundation::CFString;
    use objc2_io_kit::{
        kIOPMAssertionLevelOn, kIOReturnSuccess, IOPMAssertionCreateWithName, IOPMAssertionID,
        IOPMAssertionRelease,
    };

    use crate::Options;

    #[allow(non_upper_case_globals)]
    const kIOPMAssertionTypePreventUserIdleDisplaySleep: &str = "PreventUserIdleDisplaySleep";

    pub struct InhibitorImpl {
        #[allow(unused)]
        options: Options,
        display_assertion: IOPMAssertionID,
    }

    impl InhibitorImpl {
        pub fn new(options: Options) -> Self {
            Self {
                options,
                display_assertion: 0,
            }
        }

        #[instrument(skip_all)]
        pub fn inhibit(&mut self, reason: &str) {
            unsafe {
                let assertion_type =
                    CFString::from_static_str(kIOPMAssertionTypePreventUserIdleDisplaySleep);
                let assertion_name = CFString::from_str(reason);
                let result = IOPMAssertionCreateWithName(
                    Some(&assertion_type),
                    kIOPMAssertionLevelOn,
                    Some(&assertion_name),
                    &mut self.display_assertion,
                );
                if result != kIOReturnSuccess {
                    error!(?result, "Failed to inhibit");
                }
            }
        }

        #[instrument(skip_all)]
        pub fn un_inhibit(&mut self) {
            if self.display_assertion != 0 {
                IOPMAssertionRelease(self.display_assertion);
            }
        }
    }

    impl Drop for InhibitorImpl {
        fn drop(&mut self) {
            self.un_inhibit();
        }
    }
}

#[cfg(target_os = "linux")]
use linux::InhibitorImpl;
#[cfg(target_os = "linux")]
mod linux {
    use tracing::{error, instrument};
    use zbus::{blocking::Connection, proxy};

    use super::Options;

    #[proxy(assume_defaults = true)]
    trait ScreenSaver {
        fn inhibit(&self, application_name: &str, reason_for_inhibit: &str) -> zbus::Result<u32>;
        fn un_inhibit(&self, cookie: u32) -> zbus::Result<()>;
    }

    pub struct InhibitorImpl {
        options: Options,
        session_conn: Option<Connection>,
        screensaver_proxy: Option<ScreenSaverProxyBlocking<'static>>,
        cookie: Option<u32>,
    }

    impl InhibitorImpl {
        pub fn new(options: Options) -> Self {
            Self {
                options,
                session_conn: None,
                screensaver_proxy: None,
                cookie: None,
            }
        }

        #[instrument(skip_all)]
        pub fn inhibit(&mut self, reason: &str) {
            fn f(this: &mut InhibitorImpl, reason: &str) -> Result<(), zbus::Error> {
                this.cookie = {
                    this.session_conn = Some(Connection::session()?);
                    this.screensaver_proxy = Some(ScreenSaverProxyBlocking::new(
                        this.session_conn.as_ref().unwrap(),
                    )?);

                    Some(
                        this.screensaver_proxy
                            .as_ref()
                            .unwrap()
                            .inhibit(&this.options.app_reverse_domain, reason)?,
                    )
                };

                Ok(())
            }

            if let Err(err) = f(self, reason) {
                    error!(?err, "Failed to inhibit");
            }
        }

        #[instrument(skip_all)]
        pub fn un_inhibit(&mut self) {
            if let (Some(p), Some(cookie)) = (self.screensaver_proxy.as_ref(), self.cookie) {
                if let Err(err) = p.un_inhibit(cookie) {
                    error!(?err, "Failed to un inhibit");
                }
            }
        }
    }

    impl Drop for InhibitorImpl {
        fn drop(&mut self) {
            self.un_inhibit();
        }
    }
}
