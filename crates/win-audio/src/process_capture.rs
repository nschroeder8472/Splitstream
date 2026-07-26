//! Per-process loopback capture — `ActivateAudioInterfaceAsync` +
//! `AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK` (process-loopback-capture
//! L4, Windows 10 Build 20348+). Replaces the deleted `capture.rs`'s
//! per-endpoint bus loopback entirely: activation targets a pid directly,
//! not a device — `CapturePort` itself is unchanged, so nothing downstream
//! (the mixer, the ring, the poll loop) cares that the source changed.
//!
//! Every struct/constant name below was verified against real windows-rs
//! 0.62.2 docs (microsoft.github.io/windows-docs-rs) and the `wasapi-rs`
//! crate's own `new_application_loopback_client` implementation
//! (github.com/HEnquist/wasapi-rs, live-fetched 2026-07-21) before writing —
//! the `PROPVARIANT`/`VT_BLOB` shape is a deeply nested union with no safe
//! constructor in windows-rs itself, and getting one field wrong corrupts
//! memory, not just an error code (same caution class as `win-audio`'s
//! deleted `router.rs`, which this file replaces the reason for entirely).
//! Confirmed on real hardware (2026-07-21): a process-loopback-activated
//! `IAudioClient` does **not** implement `GetMixFormat` (`E_NOTIMPL`) — unlike
//! every other `IAudioClient` this crate opens, its format must be dictated
//! by the caller, not queried. `CAPTURE_FORMAT` below is the fixed format
//! every process capture stream uses (48kHz/stereo/float32,
//! `AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM` lets WASAPI's engine convert if the
//! underlying render path differs) — matches the Microsoft sample
//! (`Samples/ApplicationLoopback`), which hardcodes its own format for the
//! same reason rather than calling `GetMixFormat`.

use std::mem::size_of;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use windows::core::{implement, Interface, Ref, IUnknown};
use windows::Win32::Foundation::S_OK;
use windows::Win32::Media::Audio::{
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM, AUDCLNT_STREAMFLAGS_LOOPBACK, AUDIOCLIENT_ACTIVATION_PARAMS,
    AUDIOCLIENT_ACTIVATION_PARAMS_0, AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
    AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS, PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0};
use windows::Win32::System::Com::BLOB;
use windows::Win32::System::Variant::VT_BLOB;

use audio_core::Format;
use engine::ports::{CapturePort, PortError};

use crate::format::format_from_wfx;

const BUFFER_DURATION_100NS: i64 = 200_000; // 20ms shared-mode buffer hint, same as the deleted capture.rs
/// Generous ceiling for `ActivateAudioInterfaceAsync`'s completion callback —
/// real activation completes near-instantly; this only exists to fail loudly
/// instead of hanging forever on a genuinely stuck activation (review
/// finding, 2026-07-21 — see the wait loop in `open` for the full reasoning).
const ACTIVATION_TIMEOUT: Duration = Duration::from_secs(5);

/// `windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT` (value `3`,
/// verified via windows-docs-rs) — defined locally rather than pulling in
/// the whole `Win32_Media_Multimedia` Cargo feature for one well-known,
/// stable Win32 constant.
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

/// The fixed format every process capture stream is dictated to use — see
/// this module's doc comment for why (`GetMixFormat` isn't implemented on a
/// process-loopback-activated client). 32-bit float, matching every other
/// sample in this codebase's pipeline (`audio_core::Format` is always f32).
fn fixed_capture_wfx() -> WAVEFORMATEX {
    let channels = 2u16;
    let bits_per_sample = 32u16;
    let sample_rate = 48_000u32;
    let block_align = channels * (bits_per_sample / 8);
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: channels,
        nSamplesPerSec: sample_rate,
        nAvgBytesPerSec: sample_rate * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: bits_per_sample,
        cbSize: 0,
    }
}

pub struct ProcessCapture {
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

// SAFETY: same MTA-apartment argument as the deleted `WasapiCapture` and
// every other COM-holding type in this crate — every thread that touches COM
// here joins the process-wide MTA via `com::ensure_initialized` first.
unsafe impl Send for ProcessCapture {}

/// Signals the activation callback's completion across threads.
/// `ActivateAudioInterfaceAsync` is genuinely async (unlike every other WASAPI
/// call this crate makes) — the docs guarantee `ActivateCompleted` fires on
/// *some* MTA worker thread, never the calling thread, so blocking this
/// thread on the condvar the callback signals carries no deadlock risk.
#[derive(Default)]
struct ActivationState {
    done: Mutex<bool>,
    condvar: Condvar,
}

#[implement(IActivateAudioInterfaceCompletionHandler)]
struct CompletionHandler {
    state: Arc<ActivationState>,
}

impl IActivateAudioInterfaceCompletionHandler_Impl for CompletionHandler_Impl {
    fn ActivateCompleted(
        &self,
        _activate_operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        *self.state.done.lock().unwrap() = true;
        self.state.condvar.notify_one();
        Ok(())
    }
}

/// Activates a process-loopback `IAudioClient` for `pid` and blocks (bounded
/// by `ACTIVATION_TIMEOUT`) until Windows completes the activation — the one
/// genuinely async step in this whole flow. Split out from `open` (review
/// finding, 2026-07-21): activation and stream setup are two responsibilities
/// ("activate the interface" vs "initialize and build the port"), and this
/// half is also where every real-hardware surprise so far has lived
/// (`PROPVARIANT`/`ManuallyDrop`, the unbounded-wait risk).
fn activate_process_loopback(pid: u32, include_tree: bool) -> Result<IAudioClient, PortError> {
    crate::com::ensure_initialized().map_err(|e| PortError::Backend(format!("ensure_initialized: {e}")))?;

    let mode = if include_tree {
        PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE
    } else {
        PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE
    };
    // Boxed + pinned in place (not a plain stack local): `wasapi-rs`'s own
    // working implementation of this exact activation pins its params rather
    // than trusting the OS to have finished reading the blob by the time the
    // call returns — cheaper to match that caution than to assume otherwise.
    // Kept alive for this whole function (dropped only after the completion
    // wait below), well past any point Windows could still be reading it.
    let mut boxed_params = Box::pin(AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: mode,
            },
        },
    });
    // The outer `PROPVARIANT` itself (not just the inner union field) must be
    // `ManuallyDrop`-wrapped: windows-rs's `PROPVARIANT` has a real `Drop`
    // impl that calls `PropVariantClear`, which for `VT_BLOB` calls
    // `CoTaskMemFree` on `blob.pBlobData` — our pointer is Rust-`Box`-owned,
    // not `CoTaskMemAlloc`-allocated, so letting that run frees foreign
    // memory through the wrong allocator (confirmed by a real heap-corruption
    // crash — `STATUS_HEAP_CORRUPTION` — on this exact line before adding the
    // outer wrap; matches `wasapi-rs`'s own working implementation, which
    // wraps twice for the same reason).
    let prop = std::mem::ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: boxed_params.as_mut().get_mut() as *mut _ as *mut u8,
                    },
                },
            }),
        },
    });

    let state = Arc::new(ActivationState::default());
    let handler: IActivateAudioInterfaceCompletionHandler =
        CompletionHandler { state: Arc::clone(&state) }.into();

    let operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*prop as *const PROPVARIANT),
            &handler,
        )
    }
    .map_err(|e| PortError::Backend(format!("ActivateAudioInterfaceAsync: {e}")))?;

    // Bounded, not an unconditional wait (review finding, 2026-07-21): this
    // is the one genuinely async WASAPI call in the whole crate — every
    // other call here is synchronous. If Windows never fires
    // `ActivateCompleted` (stuck driver, COM edge case), an unbounded wait
    // would block this thread forever while `CaptureControl` holds the
    // engine's shared running-graph lock, freezing every other control call
    // (stats/apply_params/rebuild) on the whole engine, not just this pid's
    // capture. `ACTIVATION_TIMEOUT` is generous — real activation completes
    // near-instantly — so this only ever fires on genuine failure.
    {
        let mut done = state.done.lock().unwrap();
        loop {
            if *done {
                break;
            }
            let (guard, result) = state.condvar.wait_timeout(done, ACTIVATION_TIMEOUT).unwrap();
            done = guard;
            if result.timed_out() && !*done {
                return Err(PortError::Backend(format!(
                    "process loopback activation for pid {pid} timed out after {ACTIVATION_TIMEOUT:?}"
                )));
            }
        }
    }

    let mut result = S_OK;
    let mut activated: Option<IUnknown> = None;
    unsafe { operation.GetActivateResult(&mut result, &mut activated) }
        .map_err(|e| PortError::Backend(format!("GetActivateResult: {e}")))?;
    result
        .ok()
        .map_err(|e| PortError::Backend(format!("process loopback activation failed for pid {pid}: {e}")))?;
    activated
        .ok_or_else(|| PortError::Backend(format!("no interface returned activating process loopback for pid {pid}")))?
        .cast()
        .map_err(|e| PortError::Backend(format!("cast to IAudioClient: {e}")))
}

pub fn open(pid: u32, include_tree: bool) -> Result<ProcessCapture, PortError> {
    let client = activate_process_loopback(pid, include_tree)?;

    unsafe {
        let wfx = fixed_capture_wfx();
        let format = format_from_wfx(&wfx as *const WAVEFORMATEX);

        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
                BUFFER_DURATION_100NS,
                0,
                &wfx,
                None,
            )
            .map_err(|e| PortError::Backend(format!("Initialize: {e}")))?;

        let capture_client: IAudioCaptureClient = client
            .GetService()
            .map_err(|e| PortError::Backend(format!("GetService: {e}")))?;

        let buffer_frames = client
            .GetBufferSize()
            .map_err(|e| PortError::Backend(format!("GetBufferSize: {e}")))?;
        let channels = format.channels.max(1) as usize;
        let period_s = buffer_frames as f64 / format.sample_rate.max(1) as f64;

        client
            .Start()
            .map_err(|e| PortError::Backend(format!("Start: {e}")))?;

        Ok(ProcessCapture {
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

impl CapturePort for ProcessCapture {
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
                    break; // NORMAL when the process is silent — not an error
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

impl ProcessCapture {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to real WASAPI process-loopback activation on whatever machine
    /// runs it — not part of the normal suite (no guarantee a process with
    /// this pid is producing audio in CI). Run explicitly against a real
    /// playing process's pid:
    /// `SPLITSTREAM_TEST_PID=1234 cargo test -p win-audio -- --ignored open_and_read_a_real_process`.
    #[test]
    #[ignore]
    fn open_and_read_a_real_process() {
        let Ok(pid) = std::env::var("SPLITSTREAM_TEST_PID").and_then(|s| {
            s.parse::<u32>().map_err(|_| std::env::VarError::NotPresent)
        }) else {
            println!("SPLITSTREAM_TEST_PID not set — skipping");
            return;
        };
        let mut capture = open(pid, true).expect("open should succeed for a real, running pid");
        let mut buf = vec![0.0f32; 4096];

        // A single read() immediately after Start() almost always races
        // WASAPI's first buffer period and reads 0 regardless of whether the
        // process is producing audio — poll for up to 2s so a 0 here means
        // "genuinely silent," not "read too early."
        let mut total = 0usize;
        let mut peak = 0.0f32;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            let n = capture.read(&mut buf).expect("read");
            total += n;
            for &s in &buf[..n] {
                peak = peak.max(s.abs());
            }
            if n > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20));
            } else {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        println!(
            "captured {total} samples over 2s from pid {pid}, peak={peak}, format={:?}",
            capture.format()
        );
    }
}
