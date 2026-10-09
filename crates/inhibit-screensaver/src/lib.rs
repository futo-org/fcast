pub struct Options {
    pub app_reverse_domain: String,
}

pub struct Inhibitor {
    imp: InhibitorImpl,
    inhibited: bool,
}

impl Inhibitor {
    pub fn new(options: Options) -> Self {
        Self {
            imp: InhibitorImpl::new(options),
            inhibited: false,
        }
    }

    /// Held once however often it is asked for. A caller inhibits per item
    /// and releases per cast, and a second request on top of the first lost
    /// the first one's handle on macOS and its way back on Windows.
    pub fn inhibit(&mut self, reason: &str) {
        if edge(&mut self.inhibited, true) {
            self.imp.inhibit(reason);
        }
    }

    pub fn un_inhibit(&mut self) {
        if edge(&mut self.inhibited, false) {
            self.imp.un_inhibit();
        }
    }
}

/// Moves `held` to `want` and says whether that changed anything.
fn edge(held: &mut bool, want: bool) -> bool {
    std::mem::replace(held, want) != want
}

#[cfg(target_os = "windows")]
use windows::InhibitorImpl;
#[cfg(target_os = "windows")]
mod windows {
    use std::sync::mpsc;

    use tracing::{error, instrument};
    use windows::{
        Win32::System::Power::{
            ES_CONTINUOUS, ES_DISPLAY_REQUIRED, EXECUTION_STATE, SetThreadExecutionState,
        },
        core::Error as WindowsError,
    };

    use crate::Options;

    pub type Error = WindowsError;

    /// `SetThreadExecutionState` binds the request to the thread that made
    /// it, and the caller's task moves between runtime workers: a release
    /// from another worker left the first one holding the display on. One
    /// thread of its own holds the state, told what to hold over a channel,
    /// and its exit lets go of it.
    pub struct InhibitorImpl {
        #[allow(unused)]
        options: Options,
        holder: Option<mpsc::Sender<bool>>,
    }

    fn hold_display(rx: mpsc::Receiver<bool>) {
        for display in rx {
            let state = if display {
                ES_CONTINUOUS | ES_DISPLAY_REQUIRED
            } else {
                ES_CONTINUOUS
            };
            if unsafe { SetThreadExecutionState(state) } == EXECUTION_STATE(0) {
                error!(err = ?WindowsError::from_thread(), "Failed to set execution state");
            }
        }
    }

    impl InhibitorImpl {
        pub fn new(options: Options) -> Self {
            InhibitorImpl {
                options,
                holder: None,
            }
        }

        fn hold(&mut self, display: bool) {
            if self.holder.is_none() {
                let (tx, rx) = mpsc::channel();
                let spawned = std::thread::Builder::new()
                    .name("screensaver-inhibit".to_owned())
                    .spawn(move || hold_display(rx));
                match spawned {
                    Ok(_) => self.holder = Some(tx),
                    Err(err) => {
                        error!(?err, "No thread to hold the execution state on");
                        return;
                    }
                }
            }
            if let Some(holder) = &self.holder {
                let _ = holder.send(display);
            }
        }

        #[instrument(skip_all)]
        pub fn inhibit(&mut self, _reason: &str) {
            self.hold(true);
        }

        #[instrument(skip_all)]
        pub fn un_inhibit(&mut self) {
            // nothing was ever held without the thread
            if self.holder.is_some() {
                self.hold(false);
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
    use objc2_core_foundation::CFString;
    use objc2_io_kit::{
        IOPMAssertionCreateWithName, IOPMAssertionID, IOPMAssertionRelease, kIOPMAssertionLevelOn,
        kIOReturnSuccess,
    };
    use tracing::{error, instrument};

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
            // taken, a released id must not be released again
            let assertion = std::mem::take(&mut self.display_assertion);
            if assertion != 0 {
                IOPMAssertionRelease(assertion);
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
            // taken, a cookie is good for one release
            if let (Some(p), Some(cookie)) = (self.screensaver_proxy.as_ref(), self.cookie.take()) {
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

#[cfg(test)]
mod tests {
    use super::edge;

    /// Repeated requests reach the platform once, in either direction.
    #[test]
    fn only_a_change_reaches_the_platform() {
        let mut held = false;
        assert!(!edge(&mut held, false), "nothing to release");
        assert!(edge(&mut held, true));
        assert!(!edge(&mut held, true), "a second item of the same cast");
        assert!(held);
        assert!(edge(&mut held, false));
        assert!(!edge(&mut held, false), "an end after a stop");
        assert!(!held);
    }
}
