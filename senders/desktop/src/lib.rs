pub mod device_info_parser;
pub mod file_server;
pub mod infer;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

pub mod slint_generated {
    slint::include_modules!();
}

pub enum FetchEvent {
    Fetch,
    Quit,
}
