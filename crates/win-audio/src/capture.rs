//! Loopback capture — polled, not event-driven (spec Appendix A: loopback
//! event mode is historically unreliable). SILENT packets write zeros
//! without reading the (possibly garbage) data pointer; a zero-frame packet
//! is normal when the bus is silent, not an error (notes §3).

use std::time::Duration;

use windows::Win32::Media::Audio::{
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK,
};
use windows::Win32::System::Com::CLSCTX_ALL;

use audio_core::Format;
use engine::ports::{CapturePort, EndpointId, PortError};

const BUFFER_DURATION_100NS: i64 = 200_000; // 20ms shared-mode buffer hint

pub struct WasapiCapture {
    _client: IAudioClient, // kept alive: capture_client's buffers are borrowed from it
    capture_client: IAudioCaptureClient,
    format: Format,
    poll_interval: Duration,
    /// Leftover from a WASAPI packet that didn't fully fit in a caller's
    /// `read()` buffer. Fixed capacity (one packet's worth) set at `open()` —
    /// never reallocated on the capture (RT) thread.
    pending: Vec<f32>,
    pending_len: usize,
    pending_read: usize,
}

// SAFETY: every COM object here is created and used only within the process-
// wide MTA (`com::ensure_initialized` is called before any COM call on any
// thread that touches this). MTA objects are usable from any thread that has
// joined the same apartment — windows-rs marks COM interfaces `!Send` only
// because it can't know statically that an interface is never STA-bound;
// this crate's invariant (never STA, always MTA) makes the cross-thread move
// `open_capture` → spawned capture thread sound.
unsafe impl Send for WasapiCapture {}

pub fn open(id: &EndpointId) -> Result<WasapiCapture, PortError> {
    let device = crate::device::open(id)?;
    unsafe {
        let client: IAudioClient = device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| PortError::Backend(e.to_string()))?;

        let wfx = client
            .GetMixFormat()
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let format = Format {
            sample_rate: (*wfx).nSamplesPerSec,
            channels: (*wfx).nChannels,
        };

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                BUFFER_DURATION_100NS,
                0,
                wfx,
                None,
            )
            .map_err(|e| PortError::Backend(e.to_string()))?;
        windows::Win32::System::Com::CoTaskMemFree(Some(wfx as *const _));

        let capture_client: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| PortError::Backend(e.to_string()))?;

        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let channels = format.channels.max(1) as usize;
        let period_s = buffer_frames as f64 / format.sample_rate.max(1) as f64;

        client
            .Start()
            .map_err(|e| PortError::Backend(e.to_string()))?;

        Ok(WasapiCapture {
            _client: client,
            capture_client,
            format,
            poll_interval: Duration::from_secs_f64(period_s / 2.0), // polled at ~period/2
            pending: vec![0.0; buffer_frames as usize * channels],
            pending_len: 0,
            pending_read: 0,
        })
    }
}

impl CapturePort for WasapiCapture {
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
        let channels = self.format.channels.max(1) as usize;
        let mut written = self.drain_pending(buf);

        unsafe {
            while written < buf.len() {
                let frames_avail = self
                    .capture_client
                    .GetNextPacketSize()
                    .map_err(map_invalidated)?;
                if frames_avail == 0 {
                    break; // NORMAL when the bus is silent — not an error
                }

                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                self.capture_client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .map_err(map_invalidated)?;

                let n = frames as usize * channels;
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let room = buf.len() - written;
                let take = n.min(room);

                if silent {
                    buf[written..written + take].fill(0.0);
                } else {
                    // SILENT: data pointer may be garbage — only read when !silent.
                    let samples = std::slice::from_raw_parts(data as *const f32, n);
                    buf[written..written + take].copy_from_slice(&samples[..take]);
                }
                written += take;

                if take < n {
                    // buf ran out mid-packet — stash the remainder for the next read()
                    if silent {
                        self.pending[..n - take].fill(0.0);
                    } else {
                        let samples = std::slice::from_raw_parts(data as *const f32, n);
                        self.pending[..n - take].copy_from_slice(&samples[take..]);
                    }
                    self.pending_len = n - take;
                    self.pending_read = 0;
                }

                self.capture_client
                    .ReleaseBuffer(frames)
                    .map_err(map_invalidated)?; // ALWAYS release, even after a partial take
            }
        }

        Ok(written)
    }

    fn format(&self) -> Format {
        self.format
    }

    fn poll_interval(&self) -> Duration {
        self.poll_interval
    }
}

impl WasapiCapture {
    fn drain_pending(&mut self, buf: &mut [f32]) -> usize {
        let avail = self.pending_len - self.pending_read;
        let take = avail.min(buf.len());
        if take > 0 {
            buf[..take].copy_from_slice(&self.pending[self.pending_read..self.pending_read + take]);
            self.pending_read += take;
            if self.pending_read == self.pending_len {
                self.pending_len = 0;
                self.pending_read = 0;
            }
        }
        take
    }
}

fn map_invalidated(e: windows::core::Error) -> PortError {
    if e.code() == windows::Win32::Media::Audio::AUDCLNT_E_DEVICE_INVALIDATED {
        PortError::DeviceInvalidated
    } else {
        PortError::Backend(e.to_string())
    }
}
