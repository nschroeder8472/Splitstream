//! `IAudioEndpointVolume` on the current default render endpoint →
//! `EndpointVolumePort` (external-controls.md decision 10). Same
//! register/unregister-pair shape as `monitor.rs`'s `DeviceMonitor` —
//! explicit `Drop`-time `UnregisterControlChangeNotify`, the pair this
//! codebase has gotten wrong once and right once already.

use std::sync::mpsc::{self, Receiver, Sender};

use windows::core::{implement, GUID};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IMMDeviceEnumerator, MMDeviceEnumerator, AUDIO_VOLUME_NOTIFICATION_DATA,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use engine::ports::{EndpointVolumePort, PortError, VolumeEvent};

#[implement(IAudioEndpointVolumeCallback)]
struct VolumeSink {
    tx: Sender<VolumeEvent>,
    /// This instance's own echo-filter GUID (decision 11) — compared against
    /// `guidEventContext`, never leaked past this adapter.
    own_guid: GUID,
}

impl IAudioEndpointVolumeCallback_Impl for VolumeSink_Impl {
    fn OnNotify(&self, pnotify: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        // SAFETY: `pnotify` is valid for the duration of this call (MSDN's
        // IAudioEndpointVolumeCallback::OnNotify contract). Never call back
        // into the API from inside a notification callback (the same rule
        // `monitor.rs`'s `OnDeviceAdded` documents) — read the payload and
        // post to a channel, nothing else.
        let data = unsafe { &*pnotify };
        if data.guidEventContext == self.own_guid {
            return Ok(()); // our own write, echoed back — not a real change
        }
        let _ = self.tx.send(VolumeEvent {
            level: data.fMasterVolume,
            muted: data.bMuted.as_bool(),
        });
        Ok(())
    }
}

/// Owns the live registration; unregisters on drop.
pub struct WasapiEndpointVolume {
    volume: IAudioEndpointVolume,
    guid: GUID,
    registered: Option<IAudioEndpointVolumeCallback>,
}

// SAFETY: same reasoning as every other windows-rs COM wrapper in this crate
// — every thread touching COM joins the MTA via `com::ensure_initialized`
// first, so an MTA object is safe to use from any other MTA thread, not just
// the one that created it.
unsafe impl Send for WasapiEndpointVolume {}

impl Drop for WasapiEndpointVolume {
    fn drop(&mut self) {
        if let Some(client) = self.registered.take() {
            unsafe {
                let _ = self.volume.UnregisterControlChangeNotify(&client);
            }
        }
    }
}

pub fn open() -> Result<WasapiEndpointVolume, PortError> {
    crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        // eConsole, not eMultimedia/eCommunications — matches `default_output`'s
        // own choice (enumerator.rs) and what the OS volume mixer treats as
        // "the default device".
        let device = enumerator
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let volume: IAudioEndpointVolume = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let guid = GUID::new().map_err(|e| PortError::Backend(e.to_string()))?;
        Ok(WasapiEndpointVolume { volume, guid, registered: None })
    }
}

impl EndpointVolumePort for WasapiEndpointVolume {
    fn level(&self) -> Result<f32, PortError> {
        unsafe { self.volume.GetMasterVolumeLevelScalar() }.map_err(|e| PortError::Backend(e.to_string()))
    }

    fn set_level(&self, level: f32) -> Result<(), PortError> {
        unsafe { self.volume.SetMasterVolumeLevelScalar(level, &self.guid) }
            .map_err(|e| PortError::Backend(e.to_string()))
    }

    fn muted(&self) -> Result<bool, PortError> {
        unsafe { self.volume.GetMute() }
            .map(|b| b.as_bool())
            .map_err(|e| PortError::Backend(e.to_string()))
    }

    fn set_muted(&self, muted: bool) -> Result<(), PortError> {
        unsafe { self.volume.SetMute(muted, &self.guid) }.map_err(|e| PortError::Backend(e.to_string()))
    }

    fn take_events(&mut self) -> Receiver<VolumeEvent> {
        let (tx, rx) = mpsc::channel();
        let sink = VolumeSink { tx, own_guid: self.guid };
        let client: IAudioEndpointVolumeCallback = sink.into();
        // Best-effort (capability 7): a failed registration leaves `rx`
        // silent forever rather than erroring — same shape as
        // `SessionPort::take_events`, which also can't fail by signature.
        if unsafe { self.volume.RegisterControlChangeNotify(&client) }.is_ok() {
            self.registered = Some(client);
        }
        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Talks to the real default render endpoint on whatever machine runs
    /// it — not part of the normal suite (no audio hardware guarantee in
    /// CI). Run explicitly:
    /// `cargo test -p win-audio -- --ignored --nocapture open_and_read_real_endpoint_volume`.
    #[test]
    #[ignore]
    fn open_and_read_real_endpoint_volume() {
        let volume = open().expect("open should succeed on a machine with a default render device");
        let level = volume.level().expect("GetMasterVolumeLevelScalar");
        let muted = volume.muted().expect("GetMute");
        println!("real endpoint: level={level} muted={muted}");
        assert!((0.0..=1.0).contains(&level));
    }

    /// Registers a real callback and prints whatever notifications arrive —
    /// verifying automatically requires physically pressing a volume key
    /// while this runs, so this is a manual smoke test. Not part of the
    /// normal suite. Run explicitly:
    /// `cargo test -p win-audio -- --ignored --nocapture subscribe_and_print_real_volume_events`,
    /// then press a volume key within the sleep window.
    #[test]
    #[ignore]
    fn subscribe_and_print_real_volume_events() {
        let mut volume = open().expect("open");
        let rx = volume.take_events();
        println!("listening for volume events for 10s — press a volume key now");
        while let Ok(evt) = rx.recv_timeout(Duration::from_secs(10)) {
            println!("{evt:?}");
        }
    }
}
