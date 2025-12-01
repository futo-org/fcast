pub mod file_server;
pub mod infer;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;

pub enum FetchEvent {
    Fetch,
    Quit,
}
