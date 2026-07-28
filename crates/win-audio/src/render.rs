//! Event-driven shared-mode render — the pull clock (spec §7.1). Underflow
//! is the caller's (`engine::runtime::render_loop`) responsibility to pad
//! with silence before calling `write`; this only clips to whatever free
//! space `GetCurrentPadding` reports, it never blocks or buffers overflow.

use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    IAudioClient, IAudioRenderClient, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use audio_core::Format;
use engine::ports::{EndpointId, PortError, RenderPort};

use crate::format::format_from_wfx;

const BUFFER_DURATION_100NS: i64 = 200_000; // 20ms shared-mode buffer hint

pub struct WasapiRender {
    client: IAudioClient,
    render_client: IAudioRenderClient,
    event: HANDLE,
    format: Format,
    /// `GetBufferSize()` -- retained only as `free_frames`'s capacity basis
    /// (audio-flow-control B1); no longer reported as `period_frames()`.
    buffer_frames: u32,
    /// `GetDevicePeriod()`'s default period, in frames at `format.sample_rate`
    /// -- what one `wait_event` wakeup actually corresponds to (B1).
    period_frames: u32,
}

/// Converts a 100ns reference-time period (as `GetDevicePeriod` reports) to
/// frames at `sample_rate`. Pure, so this is testable without a device.
fn device_period_frames(period_100ns: i64, sample_rate: u32) -> u32 {
    ((period_100ns.max(0) as f64 / 10_000_000.0) * sample_rate as f64).round() as u32
}

// SAFETY: see the identical note on `WasapiCapture` — MTA-only usage makes
// the cross-thread move from `open_render` into the spawned render thread sound.
unsafe impl Send for WasapiRender {}

pub fn open(id: &EndpointId) -> Result<WasapiRender, PortError> {
    let device = crate::device::open(id)?;
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| PortError::Backend(e.to_string()))?;

        let wfx = client
            .GetMixFormat()
            .map_err(|e| PortError::Backend(e.to_string()))?;
        // Before anything reinterprets this device's buffer as `*mut f32`
        // (`write`, below): a non-float mix format would turn every write into
        // full-scale noise rather than audio, so refuse the device instead.
        if !crate::format::is_float32(wfx) {
            let bits = (*wfx).wBitsPerSample;
            let tag = (*wfx).wFormatTag;
            windows::Win32::System::Com::CoTaskMemFree(Some(wfx as *const _));
            return Err(PortError::Backend(format!(
                "unsupported mix format: {bits}-bit, wFormatTag {tag} — this build only \
                 renders 32-bit IEEE float"
            )));
        }
        let format = format_from_wfx(wfx);

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                BUFFER_DURATION_100NS,
                0,
                wfx,
                None,
            )
            .map_err(|e| PortError::Backend(e.to_string()))?;
        windows::Win32::System::Com::CoTaskMemFree(Some(wfx as *const _));

        // SetEventHandle BEFORE Start — forgetting this means Initialize
        // succeeds but no events ever fire (notes §4).
        let event = CreateEventW(None, false, false, None)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        client
            .SetEventHandle(event)
            .map_err(|e| PortError::Backend(e.to_string()))?;

        let render_client: IAudioRenderClient = client
            .GetService()
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| PortError::Backend(e.to_string()))?;

        let mut default_period_100ns: i64 = 0;
        client
            .GetDevicePeriod(Some(&mut default_period_100ns), None)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let period_frames = device_period_frames(default_period_100ns, format.sample_rate);

        client
            .Start()
            .map_err(|e| PortError::Backend(e.to_string()))?;

        Ok(WasapiRender {
            client,
            render_client,
            event,
            format,
            buffer_frames,
            period_frames,
        })
    }
}

impl RenderPort for WasapiRender {
    fn wait_event(&mut self, timeout: Duration) -> Result<(), PortError> {
        let millis = timeout.as_millis().min(u32::MAX as u128) as u32;
        let result = unsafe { WaitForSingleObject(self.event, millis) };
        if result == WAIT_OBJECT_0 {
            Ok(())
        } else {
            Err(PortError::Backend(format!(
                "wait_event timed out or failed: {result:?}"
            )))
        }
    }

    fn free_frames(&self) -> Result<usize, PortError> {
        let padding = unsafe { self.client.GetCurrentPadding().map_err(map_invalidated)? };
        Ok(self.buffer_frames.saturating_sub(padding) as usize)
    }

    fn write(&mut self, frames: &[f32]) -> Result<usize, PortError> {
        let channels = self.format.channels.max(1) as usize;
        let frame_count = (frames.len() / channels) as u32;

        let to_write = unsafe {
            let padding = self.client.GetCurrentPadding().map_err(map_invalidated)?;
            let free = self.buffer_frames.saturating_sub(padding);
            let to_write = frame_count.min(free);
            if to_write == 0 {
                return Ok(0);
            }

            let dst_ptr = self
                .render_client
                .GetBuffer(to_write)
                .map_err(map_invalidated)?;
            let dst =
                std::slice::from_raw_parts_mut(dst_ptr as *mut f32, to_write as usize * channels);
            dst.copy_from_slice(&frames[..to_write as usize * channels]);

            self.render_client
                .ReleaseBuffer(to_write, 0) // flags=0: we filled real data, not silence
                .map_err(map_invalidated)?;
            to_write
        };
        Ok(to_write as usize)
    }

    fn format(&self) -> Format {
        self.format
    }

    fn period_frames(&self) -> usize {
        self.period_frames as usize
    }
}

impl Drop for WasapiRender {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.event);
        }
    }
}

fn map_invalidated(e: windows::core::Error) -> PortError {
    if e.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED {
        PortError::DeviceInvalidated
    } else {
        PortError::Backend(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_period_frames_converts_reference_time_at_the_device_rate() {
        // 10ms @ 48kHz = 480 frames; 100ns units, so 10ms = 100_000.
        assert_eq!(device_period_frames(100_000, 48_000), 480);
        // 20.83ms @ 48kHz (a real device's commonly-reported default period)
        // rounds to 1000 frames, not truncates to 999.
        assert_eq!(device_period_frames(208_333, 48_000), 1000);
    }

    /// Talks to real WASAPI on whatever machine runs it — not part of the
    /// normal suite (no audio hardware guarantee in CI). Confirms B1's actual
    /// defect: the device *period* (`GetDevicePeriod`) is smaller than the
    /// full shared-mode buffer (`GetBufferSize`), so a caller sizing its
    /// per-event buffer from `period_frames()` must not get the buffer size
    /// back. Run explicitly:
    /// `cargo test -p win-audio -- --ignored wasapi_render_period_frames_is_the_device_period_not_the_buffer_size`.
    #[test]
    #[ignore]
    fn wasapi_render_period_frames_is_the_device_period_not_the_buffer_size() {
        let endpoints = crate::enumerator::EndpointEnumerator::new()
            .enumerate()
            .expect("enumerate should succeed on a machine with any render device");
        let endpoint = endpoints.first().expect("expected at least one render endpoint");
        let render = open(&endpoint.id).expect("open should succeed for a real render endpoint");
        println!(
            "period_frames={} buffer_frames={} format={:?}",
            render.period_frames, render.buffer_frames, render.format
        );
        assert!(
            render.period_frames <= render.buffer_frames,
            "device period must not exceed the shared-mode buffer size, got period={} buffer={}",
            render.period_frames,
            render.buffer_frames
        );
    }
}
