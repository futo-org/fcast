//! Windows-only helpers for the desktop sender.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tracing::{debug, error, warn};
use windows::{
    Win32::{
        Media::Audio::{
            Endpoints::IAudioEndpointVolume, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
            eConsole, eRender,
        },
        System::Com::{
            CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
            CoUninitialize,
        },
    },
    core::{HSTRING, PCWSTR},
};

const MUTE_MARKER_FILE: &str = "local-mute-pending";

/// COM initialised on the calling thread for the scope of a guard. The
/// MMDevice API is fine with a multithreaded apartment, and a thread that is
/// already in an apartment (S_FALSE, or RPC_E_CHANGED_MODE for an STA) can
/// use it as well; only a successful init is balanced with CoUninitialize.
struct ComApartment {
    uninit_on_drop: bool,
}

impl ComApartment {
    fn enter() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        Self {
            uninit_on_drop: hr.is_ok(),
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.uninit_on_drop {
            unsafe { CoUninitialize() };
        }
    }
}

fn device_enumerator() -> Result<IMMDeviceEnumerator> {
    unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
        .context("Failed to create the audio device enumerator")
}

fn device_id(device: &IMMDevice) -> Result<String> {
    let id = unsafe { device.GetId() }.context("Failed to get the audio device id")?;
    let owned = unsafe { id.to_string() }.context("Audio device id is not valid UTF-16");
    unsafe { CoTaskMemFree(Some(id.0 as *const _)) };
    owned
}

fn endpoint_volume(device: &IMMDevice) -> Result<IAudioEndpointVolume> {
    unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) }
        .context("Failed to open the endpoint volume control")
}

/// Mutes the default output device for the duration of a cast and puts it
/// back on drop.
///
/// Loopback capture taps the audio engine ahead of the endpoint mute on most
/// drivers, so the receiver keeps getting sound while the PC is silent; there
/// is no driver-free way on Windows to route playback away from the speakers
/// the way the Linux null sink does. Restoring goes by device id, so a
/// default-device switch mid-cast cannot leave the wrong device muted. A
/// marker file lets the next start undo a mute a crash left behind.
#[derive(Debug)]
pub struct LocalMute {
    device_id: String,
    marker: Option<PathBuf>,
}

impl LocalMute {
    /// Mutes the default output device. `Ok(None)` when it was already muted
    /// by the user, in which case there is nothing to undo later.
    pub fn engage(marker_dir: Option<&Path>) -> Result<Option<Self>> {
        let _com = ComApartment::enter();
        let device = unsafe { device_enumerator()?.GetDefaultAudioEndpoint(eRender, eConsole) }
            .context("No default output device")?;
        let device_id = device_id(&device)?;
        let volume = endpoint_volume(&device)?;

        if unsafe { volume.GetMute() }
            .context("Failed to read the mute state")?
            .as_bool()
        {
            debug!(
                device_id,
                "Output device is already muted, leaving it alone"
            );
            return Ok(None);
        }

        // Written before muting: a crash between the two just costs a
        // harmless unmute at the next start.
        let marker = marker_dir.map(|dir| dir.join(MUTE_MARKER_FILE));
        if let Some(marker) = &marker {
            if let Err(err) = std::fs::create_dir_all(marker_dir.unwrap())
                .and_then(|()| std::fs::write(marker, &device_id))
            {
                warn!(
                    ?err,
                    "Failed to write the mute marker, a crash would leave the PC muted"
                );
            }
        }

        unsafe { volume.SetMute(true, std::ptr::null()) }
            .context("Failed to mute the output device")?;
        debug!(device_id, "Muted the output device for the cast");

        Ok(Some(Self { device_id, marker }))
    }

    fn unmute(device_id: &str) -> Result<()> {
        let _com = ComApartment::enter();
        let id = HSTRING::from(device_id);
        let device = unsafe { device_enumerator()?.GetDevice(PCWSTR(id.as_ptr())) }
            .with_context(|| format!("Output device {device_id} is gone"))?;
        unsafe { endpoint_volume(&device)?.SetMute(false, std::ptr::null()) }
            .context("Failed to unmute the output device")
    }

    /// Undoes a mute that a previous run did not get to restore.
    pub fn recover(marker_dir: &Path) {
        let marker = marker_dir.join(MUTE_MARKER_FILE);
        let Ok(device_id) = std::fs::read_to_string(&marker) else {
            return;
        };
        let device_id = device_id.trim();
        warn!(
            device_id,
            "A previous run left the output device muted, unmuting"
        );
        if let Err(err) = Self::unmute(device_id) {
            error!(?err, "Failed to unmute the output device");
        }
        if let Err(err) = std::fs::remove_file(&marker) {
            warn!(?err, "Failed to remove the mute marker");
        }
    }
}

impl Drop for LocalMute {
    fn drop(&mut self) {
        match Self::unmute(&self.device_id) {
            Ok(()) => debug!(device_id = self.device_id, "Unmuted the output device"),
            Err(err) => error!(?err, "Failed to unmute the output device after the cast"),
        }
        if let Some(marker) = &self.marker
            && let Err(err) = std::fs::remove_file(marker)
        {
            warn!(?err, "Failed to remove the mute marker");
        }
    }
}
