//! The report-bug popup's content: the report the application drafted, the
//! user's checklist over it, and the QR of the link the fitted report makes.
//! The format itself, its encoding and the decoder, is `fcast_bug_report`.

pub use fcast_bug_report::{Fit, ISSUE_TRACKER_URL, Report};

use crate::ui_types::QrCode;

/// Bytes a QR still scans from a couch at level M, roughly version 20.
pub const QR_BUDGET: usize = 650;

/// The full report as drafted, before the checklist. `report` carries the
/// media address with its path, `host` is the same location without it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Draft {
    pub report: Report,
    pub host: String,
}

/// The user's checklist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Include {
    pub source: bool,
    pub source_path: bool,
    pub sender: bool,
    pub device: bool,
    pub warnings: bool,
}

impl Default for Include {
    fn default() -> Self {
        Self {
            source: true,
            source_path: false,
            sender: true,
            device: true,
            warnings: true,
        }
    }
}

impl Draft {
    /// The report the checklist allows. The error, its message and the
    /// receiver version are not the user's to drop.
    pub fn build(&self, include: Include) -> Report {
        let mut report = self.report.clone();
        if !include.device {
            report.device = Default::default();
        }
        match report.source.as_mut() {
            Some(_) if !include.source => report.source = None,
            Some(source) if !include.source_path => source.location = self.host.clone(),
            _ => {}
        }
        if !include.sender {
            report.sender = None;
        }
        if !include.warnings {
            report.warnings.clear();
        }
        report
    }

    /// The link the QR carries, fitted to the QR budget, and what the fit
    /// left out.
    pub fn issue_url(&self, include: Include) -> (String, Fit) {
        let (fitted, fit) = self.build(include).fitted(QR_BUDGET);
        (fitted.issue_url(), fit)
    }
}

/// A QR of `text` at level M, the level a photographed screen needs.
pub fn qr_for(text: &str) -> Option<QrCode> {
    let qrcode = fast_qr::QRBuilder::new(text.as_bytes())
        .ecl(fast_qr::ECL::M)
        .build()
        .ok()?;
    let dims = qrcode.size as u32;
    let module_count = (dims * dims) as usize;
    let dark = qrcode.data[0..module_count]
        .iter()
        .map(|module| *module != fast_qr::Module::LIGHT)
        .collect();
    Some(QrCode { size: dims, dark })
}

/// This build's OS and arch, and the display server the window runs on.
pub fn device() -> fcast_bug_report::Device {
    use fcast_bug_report::{Arch, Display, Os};
    let os = match std::env::consts::OS {
        "linux" => Os::Linux,
        "android" => Os::Android,
        "macos" => Os::Macos,
        "windows" => Os::Windows,
        _ => Os::Unknown,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => Arch::X86_64,
        "aarch64" => Arch::Aarch64,
        "x86" => Arch::X86,
        "arm" => Arch::Arm,
        _ => Arch::Unknown,
    };
    let display = if os == Os::Linux {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            Display::Wayland
        } else if std::env::var_os("DISPLAY").is_some() {
            Display::X11
        } else {
            Display::Unknown
        }
    } else {
        Display::Native
    };
    fcast_bug_report::Device {
        os,
        arch,
        display,
        model: system_property(c"ro.product.model"),
        os_version: system_property(c"ro.build.version.release"),
    }
}

/// An android system property, the model and release a TV bug most needs.
/// Empty on the other platforms, which have no such table.
#[cfg(target_os = "android")]
pub(crate) fn system_property(name: &std::ffi::CStr) -> String {
    // PROP_VALUE_MAX
    let mut value = [0 as libc::c_char; 92];
    // SAFETY: the name is a NUL-terminated C string and the buffer is
    // PROP_VALUE_MAX bytes, the most the property API writes.
    let len = unsafe { libc::__system_property_get(name.as_ptr(), value.as_mut_ptr()) };
    if len <= 0 {
        return String::new();
    }
    let bytes: Vec<u8> = value[..len as usize].iter().map(|c| *c as u8).collect();
    String::from_utf8_lossy(&bytes).trim().to_owned()
}

#[cfg(not(target_os = "android"))]
pub(crate) fn system_property(_name: &std::ffi::CStr) -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use fcast_bug_report::{Code, Sender, Source, SourceKind, Warning};

    fn draft() -> Draft {
        Draft {
            report: Report {
                code: Code::error(99),
                message: "simulated".to_owned(),
                receiver_version: "3.0.4".to_owned(),
                device: device(),
                source: Some(Source {
                    kind: SourceKind::Single,
                    container: "video/mp4".to_owned(),
                    location: "https://example.com/private/movie.mp4".to_owned(),
                }),
                sender: Some(Sender {
                    app: "Grayjay".to_owned(),
                    version: "2.0".to_owned(),
                    protocol: fcast_bug_report::Protocol::FcastV4,
                }),
                warnings: vec![Warning {
                    secs_ago: 3,
                    code: Code::warning(1),
                    repeats: 1,
                    message: "stalled".to_owned(),
                }],
                ..Report::default()
            },
            host: "example.com".to_owned(),
        }
    }

    #[test]
    fn the_default_checklist_names_the_host_not_the_path() {
        let text = draft().build(Include::default()).render();
        assert!(text.contains("source single video/mp4 example.com"), "{text}");
        assert!(!text.contains("private/movie"), "{text}");
        assert!(text.contains("sender Grayjay 2.0, fcast v4"), "{text}");
        assert!(text.ends_with("3s ago FC-W01 stalled"), "{text}");
    }

    #[test]
    fn the_path_toggle_swaps_the_location() {
        let text = draft()
            .build(Include {
                source_path: true,
                ..Include::default()
            })
            .render();
        assert!(text.contains("source single video/mp4 https://example.com/private/movie.mp4"), "{text}");
    }

    #[test]
    fn everything_off_leaves_the_error_and_the_version() {
        let text = draft()
            .build(Include {
                source: false,
                source_path: false,
                sender: false,
                device: false,
                warnings: false,
            })
            .render();
        assert_eq!(text, "FC-E99 Unexpected\nsimulated\nreceiver 3.0.4");
    }

    #[test]
    fn the_link_decodes_to_the_report_the_checklist_built() {
        let draft = draft();
        let (url, fit) = draft.issue_url(Include::default());
        assert_eq!(fit.warnings_kept, 1);
        let decoded = fcast_bug_report::find_and_decode(&url).unwrap();
        assert_eq!(decoded, draft.build(Include::default()));
        assert!(qr_for(&url).is_some());
    }

    #[test]
    fn this_build_names_its_platform() {
        let device = device();
        assert!(!device.is_unknown());
        assert_ne!(device.os, fcast_bug_report::Os::Unknown);
    }
}
