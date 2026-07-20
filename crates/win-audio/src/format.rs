//! Endpoint mix-format probing, shared by the enumerator (capability probe)
//! and capture/render (`open()`-time format negotiation, notes §3).

use windows::Win32::Media::Audio::{IAudioClient, IMMDevice};
use windows::Win32::System::Com::CLSCTX_ALL;

use audio_core::Format;
use engine::ports::PortError;

/// Activates a throwaway `IAudioClient` just to read `GetMixFormat()`.
/// Non-exclusive, doesn't start the stream — safe to call for capability
/// probing without disturbing any other client of the device (spec: never
/// open a physical endpoint in exclusive mode).
pub fn client_mix_format(device: &IMMDevice) -> Result<Format, PortError> {
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        wave_format(&client)
    }
}

unsafe fn wave_format(client: &IAudioClient) -> Result<Format, PortError> {
    let wfx = client
        .GetMixFormat()
        .map_err(|e| PortError::Backend(e.to_string()))?;
    let format = Format {
        sample_rate: (*wfx).nSamplesPerSec,
        channels: (*wfx).nChannels,
    };
    windows::Win32::System::Com::CoTaskMemFree(Some(wfx as *const _));
    Ok(format)
}
