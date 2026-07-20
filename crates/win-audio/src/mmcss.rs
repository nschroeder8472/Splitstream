//! `AvSetMmThreadCharacteristics("Pro Audio")` — the real producer behind
//! `engine::ports::RtGuard`. Promotion failure degrades gracefully (a normal-
//! priority audio thread is still functional, just more glitch-prone) rather
//! than failing the whole engine.

use windows::core::PCWSTR;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Threading::{
    AvRevertMmThreadCharacteristics, AvSetMmThreadCharacteristicsW,
};

use engine::ports::RtGuard;

/// Wrapper purely to make `HANDLE` usable inside the `FnOnce() + Send` that
/// `RtGuard::new` requires — `HANDLE` itself is a plain `isize` newtype and
/// carries no thread affinity of its own (unlike `ComGuard`).
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

/// Also joins this thread to the COM MTA (see `com::ensure_initialized`) —
/// every RT thread that will make WASAPI calls needs both, and this is the
/// one place `engine::runtime` calls before each thread's loop starts.
pub fn promote_current_thread() -> RtGuard {
    if crate::com::ensure_initialized().is_err() {
        return RtGuard::noop(); // can't safely make any WASAPI call on this thread
    }

    let name: Vec<u16> = "Pro Audio\0".encode_utf16().collect();
    let mut task_index = 0u32;

    match unsafe { AvSetMmThreadCharacteristicsW(PCWSTR(name.as_ptr()), &mut task_index) } {
        Ok(handle) => {
            let handle = SendHandle(handle);
            RtGuard::new(move || {
                let handle = handle; // move into the closure body
                let _ = unsafe { AvRevertMmThreadCharacteristics(handle.0) };
            })
        }
        Err(_) => RtGuard::noop(),
    }
}
