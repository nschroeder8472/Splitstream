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

/// `WAVE_FORMAT_IEEE_FLOAT` (mmreg.h value `3`) and the matching
/// `KSDATAFORMAT_SUBTYPE_IEEE_FLOAT` GUID (ksmedia.h), both defined locally
/// rather than pulling in the whole `Win32_Media_Multimedia` Cargo feature for
/// two well-known, stable Win32 constants — same precedent as
/// `process_capture.rs`.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: windows::core::GUID =
    windows::core::GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Whether `wfx` describes 32-bit IEEE float samples — the one and only layout
/// every buffer in this crate reinterprets a raw WASAPI pointer as
/// (`render.rs`'s `dst_ptr as *mut f32`, `process_capture.rs`'s
/// `data as *const f32`). Windows' shared-mode engine format is float32 in
/// practice regardless of the bit depth chosen in the Sound control panel
/// (that setting drives the driver side, not the shared-mode client format),
/// but "in practice" is not "guaranteed": a device reporting 16- or 24-bit
/// PCM here would have its bytes reinterpreted as floats, i.e. full-scale
/// noise. Checked at `open` so that fails loudly instead.
///
/// # Safety
/// Same contract as [`format_from_wfx`].
pub(crate) unsafe fn is_float32(wfx: *const WAVEFORMATEX) -> bool {
    if (*wfx).wBitsPerSample != 32 {
        return false;
    }
    match (*wfx).wFormatTag as u32 {
        t if t == WAVE_FORMAT_IEEE_FLOAT as u32 => true,
        WAVE_FORMAT_EXTENSIBLE if (*wfx).cbSize >= 22 => {
            let ext = wfx as *const WAVEFORMATEXTENSIBLE;
            // `WAVEFORMATEXTENSIBLE` is `#[repr(packed)]`, so `SubFormat` may
            // be unaligned — copy it out rather than taking a reference to it.
            let sub = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
            sub == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    }
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
