//! Endpoint mix-format probing, shared by the enumerator (capability probe)
//! and capture/render (`open()`-time format negotiation, notes §3). Also
//! the single place that turns a raw `WAVEFORMATEX*` into a `Format` with a
//! real channel layout (notes §17) — `capture.rs` and `render.rs` call
//! [`format_from_wfx`] instead of re-deriving `Format` themselves, so the
//! `dwChannelMask` read exists in exactly one place, not three.

use windows::Win32::Media::Audio::{IAudioClient, IMMDevice, WAVEFORMATEX, WAVEFORMATEXTENSIBLE};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::System::Com::CLSCTX_ALL;

use audio_core::{ChannelLayout, Format};
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
    let format = format_from_wfx(wfx);
    windows::Win32::System::Com::CoTaskMemFree(Some(wfx as *const _));
    Ok(format)
}

/// Reads sample rate, channel count, and speaker layout from a raw
/// `WAVEFORMATEX*` (as returned by `GetMixFormat` / `Initialize`'s in/out
/// param). `dwChannelMask` only exists on the `WAVEFORMATEXTENSIBLE`
/// extension — check `wFormatTag`/`cbSize` before reinterpreting the
/// pointer. A plain `WAVEFORMATEX`, or a mask that's zero/inconsistent with
/// `nChannels` (both happen on real drivers), falls back to
/// `ChannelLayout::default_for_count` rather than trusting a bad mask.
///
/// # Safety
/// `wfx` must point to a valid `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE`
/// when `wFormatTag == WAVE_FORMAT_EXTENSIBLE`), as returned by WASAPI.
pub(crate) unsafe fn format_from_wfx(wfx: *const WAVEFORMATEX) -> Format {
    let channels = (*wfx).nChannels;
    let sample_rate = (*wfx).nSamplesPerSec;

    let is_extensible =
        (*wfx).wFormatTag as u32 == WAVE_FORMAT_EXTENSIBLE && (*wfx).cbSize >= 22;
    let layout = if is_extensible {
        let ext = wfx as *const WAVEFORMATEXTENSIBLE;
        ChannelLayout::from_mask((*ext).dwChannelMask, channels)
    } else {
        ChannelLayout::default_for_count(channels)
    };

    Format {
        sample_rate,
        channels,
        layout,
    }
}
