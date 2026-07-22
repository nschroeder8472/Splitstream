//! `IAudioSessionManager2` → `SessionPort` (session-routing L4, notes §14).
//! Sessions are enumerated/notified per-endpoint, not globally — a session
//! can be live on any active render endpoint, not just the default one. So
//! this scans every endpoint the enumerator reports, not just the default
//! (session-routing 2026-07-20 decision), merging by pid. Unaffected by the
//! process-loopback-capture pivot — session discovery (which processes are
//! playing audio) is a separate concern from where their audio is captured
//! from.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use windows::core::{implement, Interface};
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::{
    AudioSessionDisconnectReason, AudioSessionState, AudioSessionStateExpired,
    IAudioSessionControl, IAudioSessionControl2, IAudioSessionEvents, IAudioSessionEvents_Impl,
    IAudioSessionManager2, IAudioSessionNotification, IAudioSessionNotification_Impl,
    ISimpleAudioVolume,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

use engine::ports::{EndpointId, PortError, SessionEvent, SessionPort};
use engine::SessionInfo;

use crate::enumerator::EndpointEnumerator;

/// Per-session state-change registration, kept alive for the whole
/// coordinator lifetime. `Drop` below explicitly unregisters — the manager/
/// control holds its own `AddRef`'d reference to the registered callback, so
/// merely dropping our local handles would leave the registration live on
/// WASAPI's side indefinitely (same RAII shape as `monitor::DeviceMonitor`,
/// which has to do the same for `IMMNotificationClient`). A named struct,
/// not a raw tuple: windows-rs COM interface wrappers are `!Send`/`!Sync` by
/// default, and a tuple of foreign types can't get a local unsafe impl
/// (orphan rule) — this crate owns the wrapper instead.
struct SessionRegistration {
    control: IAudioSessionControl,
    events: IAudioSessionEvents,
}

// SAFETY: same reasoning as `WasapiCapture`/`DeviceMonitor` — every thread
// touching COM in this crate joins the MTA first via `com::ensure_initialized`;
// an MTA object is safe to use from any thread that's joined the same
// apartment. Access is always behind the `Mutex` below, so no concurrent
// access to worry about beyond the cross-thread move itself.
unsafe impl Send for SessionRegistration {}
unsafe impl Sync for SessionRegistration {}

impl Drop for SessionRegistration {
    fn drop(&mut self) {
        unsafe {
            let _ = self.control.UnregisterAudioSessionNotification(&self.events);
        }
    }
}

/// Owns one endpoint's new-session registration. `Drop` unregisters — same
/// leak-on-drop hazard and same fix as `SessionRegistration` above.
struct ManagerRegistration {
    manager: IAudioSessionManager2,
    notification: IAudioSessionNotification,
}

// SAFETY: same reasoning as `SessionRegistration` above.
unsafe impl Send for ManagerRegistration {}

impl Drop for ManagerRegistration {
    fn drop(&mut self) {
        unsafe {
            let _ = self.manager.UnregisterSessionNotification(&self.notification);
        }
    }
}

pub struct WasapiSessions {
    enumerator: EndpointEnumerator,
    session_registrations: Arc<Mutex<Vec<SessionRegistration>>>,
    manager_registrations: Vec<ManagerRegistration>,
}

// SAFETY: same MTA-apartment argument as `WasapiCapture`/`DeviceMonitor` —
// every thread that touches COM in this crate joins the process-wide MTA via
// `com::ensure_initialized` first.
unsafe impl Send for WasapiSessions {}

impl WasapiSessions {
    pub fn new() -> WasapiSessions {
        WasapiSessions {
            enumerator: EndpointEnumerator::new(),
            session_registrations: Arc::new(Mutex::new(Vec::new())),
            manager_registrations: Vec::new(),
        }
    }
}

impl Default for WasapiSessions {
    fn default() -> WasapiSessions {
        WasapiSessions::new()
    }
}

impl SessionPort for WasapiSessions {
    fn enumerate(&mut self) -> Result<Vec<SessionInfo>, PortError> {
        crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
        let endpoints = self.enumerator.enumerate()?;

        // Dedup by pid: the same session can legitimately appear on more than
        // one endpoint's manager only in edge cases, but a stable merge here
        // keeps `enumerate()` idempotent regardless.
        let mut sessions: HashMap<u32, SessionInfo> = HashMap::new();
        for endpoint in &endpoints {
            let Ok(manager) = activate_manager(&endpoint.id) else {
                continue; // best-effort per endpoint (notes §14): one bad endpoint
                          // (e.g. mid-teardown) shouldn't fail the whole enumerate
            };
            if let Ok(list) = enumerate_sessions(&manager) {
                for info in list {
                    sessions.entry(info.pid).or_insert(info);
                }
            }
        }
        Ok(sessions.into_values().collect())
    }

    /// Single-consume, same pattern as `AudioSystem::subscribe_device_events`:
    /// a second call replaces all prior manager/session registrations.
    fn take_events(&mut self) -> Receiver<SessionEvent> {
        let (tx, rx) = mpsc::channel();
        self.manager_registrations.clear();
        self.session_registrations.lock().unwrap().clear();

        let Ok(endpoints) = self.enumerator.enumerate() else {
            return rx;
        };
        for endpoint in &endpoints {
            let Ok(manager) = activate_manager(&endpoint.id) else {
                continue;
            };
            // Register per-session ended-tracking for every session already
            // live on this endpoint before we start listening for new ones —
            // otherwise a session that existed before `take_events()` was
            // called could never be detected as ended.
            if let Ok(session_enum) = unsafe { manager.GetSessionEnumerator() } {
                if let Ok(count) = unsafe { session_enum.GetCount() } {
                    for i in 0..count {
                        if let Ok(control) = unsafe { session_enum.GetSession(i) } {
                            if let Some(pid) = session_pid(&control) {
                                register_session_events(&control, pid, &tx, &self.session_registrations);
                            }
                        }
                    }
                }
            }

            if let Ok(notification) = register_new_session_notifications(
                &manager,
                tx.clone(),
                Arc::clone(&self.session_registrations),
            ) {
                self.manager_registrations.push(ManagerRegistration {
                    manager,
                    notification,
                });
            }
        }
        rx
    }

    /// Scans every render endpoint's session manager (same multi-endpoint
    /// reasoning as `enumerate`/`take_events` above) for the live session
    /// whose pid matches, casts to `ISimpleAudioVolume`, calls `SetMute`.
    /// Pid not found on any endpoint (already exited) is `Ok(())` — best-effort,
    /// per the trait's documented contract.
    fn set_muted(&self, pid: u32, muted: bool) -> Result<(), PortError> {
        crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
        let endpoints = self.enumerator.enumerate()?;
        for endpoint in &endpoints {
            let Ok(manager) = activate_manager(&endpoint.id) else {
                continue;
            };
            let Some(control) = find_session_control(&manager, pid) else {
                continue;
            };
            let volume: ISimpleAudioVolume = control
                .cast()
                .map_err(|e| PortError::Backend(e.to_string()))?;
            unsafe {
                volume
                    .SetMute(muted, std::ptr::null())
                    .map_err(|e| PortError::Backend(e.to_string()))?;
            }
            return Ok(());
        }
        Ok(())
    }
}

/// Finds the live session control whose pid matches, scanning `manager`'s
/// session enumerator. Best-effort per-session lookup failures are skipped,
/// not propagated — same posture as `enumerate_sessions`.
fn find_session_control(manager: &IAudioSessionManager2, pid: u32) -> Option<IAudioSessionControl> {
    unsafe {
        let session_enum = manager.GetSessionEnumerator().ok()?;
        let count = session_enum.GetCount().unwrap_or(0);
        for i in 0..count {
            if let Ok(control) = session_enum.GetSession(i) {
                if session_pid(&control) == Some(pid) {
                    return Some(control);
                }
            }
        }
    }
    None
}

fn activate_manager(id: &EndpointId) -> Result<IAudioSessionManager2, PortError> {
    let device = crate::device::open(id)?;
    unsafe {
        device
            .Activate(CLSCTX_ALL, None)
            .map_err(|e| PortError::Backend(e.to_string()))
    }
}

fn enumerate_sessions(manager: &IAudioSessionManager2) -> Result<Vec<SessionInfo>, PortError> {
    unsafe {
        let session_enum = manager
            .GetSessionEnumerator()
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let count = session_enum
            .GetCount()
            .map_err(|e| PortError::Backend(e.to_string()))?;

        let mut result = Vec::with_capacity(count.max(0) as usize);
        for i in 0..count {
            let control = session_enum
                .GetSession(i)
                .map_err(|e| PortError::Backend(e.to_string()))?;
            if let Some(info) = describe_session(&control) {
                result.push(info);
            }
        }
        Ok(result)
    }
}

/// `None` for the system-sounds session (pid 0 — notes §14 gotcha 4) or if
/// the process ID itself can't be read.
fn session_pid(control: &IAudioSessionControl) -> Option<u32> {
    let control2: IAudioSessionControl2 = control.cast().ok()?;
    let pid = unsafe { control2.GetProcessId() }.ok()?;
    (pid != 0).then_some(pid)
}

/// `None` for the system-sounds session or if the process can no longer be
/// queried (already exited between enumeration and description — not an
/// error, just skip it).
fn describe_session(control: &IAudioSessionControl) -> Option<SessionInfo> {
    let pid = session_pid(control)?;
    let display_name = unsafe { control.GetDisplayName() }
        .ok()
        .map(pwstr_to_string)
        .unwrap_or_default();
    let process_path = process_image_path(pid).unwrap_or_default();
    Some(SessionInfo {
        pid,
        process_path,
        display_name,
    })
}

/// Best-effort: `OpenProcess`/`QueryFullProcessImageNameW` failing (no
/// permission, process already gone) leaves the session unmatchable by path
/// rather than erroring the whole describe — `match_session` just won't
/// match anything for an empty path.
fn process_image_path(pid: u32) -> Option<PathBuf> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let result = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        );
        let _ = CloseHandle(handle);
        result.ok()?;
        Some(PathBuf::from(String::from_utf16_lossy(&buf[..len as usize])))
    }
}

fn pwstr_to_string(s: windows::core::PWSTR) -> String {
    let result = unsafe { s.to_string() }.unwrap_or_default();
    unsafe { windows::Win32::System::Com::CoTaskMemFree(Some(s.0 as *const _)) };
    result
}

/// Registers per-session ended-tracking on `control` and stashes the
/// registration in `registrations` to keep it alive. `pid` is the caller's
/// job to supply (already known from `describe_session`/`session_pid` —
/// avoids a second COM round-trip to re-derive it here). No-op if
/// registration fails — best-effort, same posture as the rest of this module.
fn register_session_events(
    control: &IAudioSessionControl,
    pid: u32,
    tx: &Sender<SessionEvent>,
    registrations: &Arc<Mutex<Vec<SessionRegistration>>>,
) {
    let sink = StateSink {
        tx: tx.clone(),
        pid,
    };
    let events: IAudioSessionEvents = sink.into();
    if unsafe { control.RegisterAudioSessionNotification(&events) }.is_ok() {
        registrations
            .lock()
            .unwrap()
            .push(SessionRegistration {
                control: control.clone(),
                events,
            });
    }
}

#[implement(IAudioSessionNotification)]
struct NewSessionSink {
    tx: Sender<SessionEvent>,
    registrations: Arc<Mutex<Vec<SessionRegistration>>>,
}

impl IAudioSessionNotification_Impl for NewSessionSink_Impl {
    /// GOTCHA 3 (notes §14): extract from the given control and push — no
    /// calling back into `IAudioSessionManager2` from inside this callback.
    /// `RegisterAudioSessionNotification` below operates on the control
    /// that's already been handed to us, not the manager, so it's safe here.
    fn OnSessionCreated(
        &self,
        new_session: windows::core::Ref<'_, IAudioSessionControl>,
    ) -> windows::core::Result<()> {
        let Some(control) = new_session.as_ref() else {
            return Ok(());
        };
        let Some(info) = describe_session(control) else {
            return Ok(());
        };
        register_session_events(control, info.pid, &self.tx, &self.registrations);
        let _ = self.tx.send(SessionEvent::New(info));
        Ok(())
    }
}

fn register_new_session_notifications(
    manager: &IAudioSessionManager2,
    tx: Sender<SessionEvent>,
    registrations: Arc<Mutex<Vec<SessionRegistration>>>,
) -> windows::core::Result<IAudioSessionNotification> {
    let sink = NewSessionSink { tx, registrations };
    let notification: IAudioSessionNotification = sink.into();
    unsafe { manager.RegisterSessionNotification(&notification) }?;
    Ok(notification)
}

#[implement(IAudioSessionEvents)]
struct StateSink {
    tx: Sender<SessionEvent>,
    pid: u32,
}

impl IAudioSessionEvents_Impl for StateSink_Impl {
    fn OnStateChanged(&self, new_state: AudioSessionState) -> windows::core::Result<()> {
        if new_state == AudioSessionStateExpired {
            let _ = self.tx.send(SessionEvent::Ended(self.pid));
        }
        Ok(())
    }

    fn OnSessionDisconnected(
        &self,
        _disconnect_reason: AudioSessionDisconnectReason,
    ) -> windows::core::Result<()> {
        let _ = self.tx.send(SessionEvent::Ended(self.pid));
        Ok(())
    }

    fn OnDisplayNameChanged(
        &self,
        _new_display_name: &windows::core::PCWSTR,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(()) // not a match target (session-routing.md decision) — ignored
    }

    fn OnIconPathChanged(
        &self,
        _new_icon_path: &windows::core::PCWSTR,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnSimpleVolumeChanged(
        &self,
        _new_volume: f32,
        _new_mute: windows::core::BOOL,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnChannelVolumeChanged(
        &self,
        _channel_count: u32,
        _new_channel_volume_array: *const f32,
        _changed_channel: u32,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }

    fn OnGroupingParamChanged(
        &self,
        _new_grouping_param: *const windows::core::GUID,
        _event_context: *const windows::core::GUID,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to real WASAPI — not part of the normal suite (no session
    /// guarantee in CI: needs at least one app actually playing audio). Run
    /// explicitly: `cargo test -p win-audio -- --ignored enumerate_real_sessions`.
    #[test]
    #[ignore]
    fn enumerate_real_sessions() {
        let mut sessions = WasapiSessions::new();
        let list = sessions.enumerate().expect("enumerate");
        for s in &list {
            println!("pid={} path={:?} name={:?}", s.pid, s.process_path, s.display_name);
        }
    }

    /// Manual smoke test — verifying "ended" detection automatically requires
    /// starting/stopping real playback, so this just prints whatever arrives.
    /// Run explicitly: `cargo test -p win-audio -- --ignored --nocapture
    /// subscribe_and_print_real_session_events`, then start/stop audio in
    /// another app within the sleep window.
    #[test]
    #[ignore]
    fn subscribe_and_print_real_session_events() {
        let mut sessions = WasapiSessions::new();
        let rx = sessions.take_events();
        println!("listening for session events for 15s — start/stop audio in another app now");
        while let Ok(evt) = rx.recv_timeout(std::time::Duration::from_secs(15)) {
            println!("{evt:?}");
        }
    }

    /// Manual smoke test (session-mute-on-capture) — mutes the first live
    /// session found for 3s (verify in Volume Mixer / listen for silence),
    /// then unmutes it. Run explicitly: `cargo test -p win-audio -- --ignored
    /// --nocapture mute_and_unmute_a_real_session`, with some app already
    /// playing audio.
    #[test]
    #[ignore]
    fn mute_and_unmute_a_real_session() {
        let mut sessions = WasapiSessions::new();
        let list = sessions.enumerate().expect("enumerate");
        let target = list.first().expect("need at least one live session playing audio");
        println!("muting pid={} name={:?} for 3s...", target.pid, target.display_name);
        sessions.set_muted(target.pid, true).expect("mute");
        std::thread::sleep(std::time::Duration::from_secs(3));
        println!("unmuting pid={}", target.pid);
        sessions.set_muted(target.pid, false).expect("unmute");
    }
}
