//! COM apartment guard. Every `win-audio` thread that touches COM calls
//! [`ComGuard::init_mta`] first and holds the guard for the thread's whole life.

use std::cell::RefCell;
use std::marker::PhantomData;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// `!Send`/`!Sync` by design (the `*const ()` marker, not the empty tuple):
/// a COM apartment is thread-affine, so this guard must never cross threads.
pub struct ComGuard(PhantomData<*const ()>);

impl ComGuard {
    pub fn init_mta() -> windows::core::Result<ComGuard> {
        // S_FALSE (already initialized on this thread) is still Ok here — it
        // must still be balanced with CoUninitialize on drop.
        unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok()? };
        Ok(ComGuard(PhantomData))
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() }
    }
}

thread_local! {
    static COM: RefCell<Option<ComGuard>> = const { RefCell::new(None) };
}

/// Lazily joins the MTA on the calling thread the first time it's called,
/// and keeps that guard alive for the thread's whole life (dropped, running
/// `CoUninitialize`, when the thread's `thread_local` storage is torn down
/// at thread exit). `WasapiSystem` can't hold a `ComGuard` itself — it's
/// `!Send` by design, and `AudioSystem: Send + Sync` — so every method that
/// makes COM calls (`enumerate`, `open_capture`, `open_render`, and MMCSS
/// promotion on each spawned RT thread) calls this first instead.
pub fn ensure_initialized() -> windows::core::Result<()> {
    COM.with(|cell| {
        let mut slot = cell.borrow_mut();
        if slot.is_none() {
            *slot = Some(ComGuard::init_mta()?);
        }
        Ok(())
    })
}
