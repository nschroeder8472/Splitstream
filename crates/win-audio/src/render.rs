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
    buffer_frames: u32,
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

        client
            .Start()
            .map_err(|e| PortError::Backend(e.to_string()))?;

        Ok(WasapiRender {
            client,
            render_client,
            event,
            format,
            buffer_frames,
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

    fn write(&mut self, frames: &[f32]) -> Result<(), PortError> {
        let channels = self.format.channels.max(1) as usize;
        let frame_count = (frames.len() / channels) as u32;

        unsafe {
            let padding = self.client.GetCurrentPadding().map_err(map_invalidated)?;
            let free = self.buffer_frames.saturating_sub(padding);
            let to_write = frame_count.min(free);
            if to_write == 0 {
                return Ok(());
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
        }
        Ok(())
    }

    fn format(&self) -> Format {
        self.format
    }

    fn period_frames(&self) -> usize {
        self.buffer_frames as usize
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
