//! Undocumented WASAPI/WinRT policy surfaces → `PolicyPort` (session-routing
//! L4, notes §13). Behind the `policy-routing` Cargo feature. Every GUID and
//! vtable slot below was pulled from EarTrumpet's source
//! (github.com/File-New-Project/EarTrumpet, `Interop/MMDeviceAPI/`) on
//! 2026-07-20, not from memory — a wrong slot order corrupts memory, not
//! just returns an error code, so this is the one file in the crate that
//! must never be edited from recollection; re-fetch and re-verify against
//! EarTrumpet if any of these interfaces ever need to change.
//!
//! Two separate undocumented surfaces:
//! - `IPolicyConfigWin7` (classic COM, `CoCreateInstance` on CLSID
//!   `PolicyConfigClient`) — endpoint visibility + the *system* default
//!   output: `set_visibility`/`set_default`.
//! - `AudioPolicyConfig` (WinRT, `RoGetActivationFactory` on runtime class
//!   `"Windows.Media.Internal.AudioPolicyConfig"`) — the *per-app* persisted
//!   default endpoint: `route`/`clear_route`. Its interface IID changed
//!   between Windows builds (pre/post 21H2); tries the new IID first, falls
//!   back to the old one, else `PolicyError::Unavailable`.
//!
//! Every call is best-effort per spec §9.3–9.4: HRESULT failure or
//! activation failure both become a `PolicyError`, never a panic, never a
//! retry (`routing.rs`'s degradation posture owns retry policy).

use windows::core::{interface, IUnknown, IUnknown_Vtbl, HRESULT, HSTRING, GUID, PCWSTR};
use windows::Win32::Media::Audio::{eConsole, eRender, EDataFlow, ERole};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};
use windows::Win32::System::WinRT::RoGetActivationFactory;

use engine::ports::{EndpointId, PolicyError, PolicyPort};

/// CLSID `PolicyConfigClient` (EarTrumpet: `Interop/MMDeviceAPI/PolicyConfigClient.cs`).
const POLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870AF99C_171D_4F9E_AF0D_E63DF40C2BC9);

/// IID `F8679F50-850A-41CF-9C72-430F290290C8`. 8 unused vtable slots, then
/// `GetPropertyValue`/`SetPropertyValue` (also never called — placeholder'd
/// too), then the two methods this module actually uses. Exact shape from
/// EarTrumpet's `IPolicyConfig.cs` (`IPolicyConfigWin7`).
#[interface("f8679f50-850a-41cf-9c72-430f290290c8")]
unsafe trait IPolicyConfigWin7: IUnknown {
    unsafe fn _unused1(&self) -> HRESULT;
    unsafe fn _unused2(&self) -> HRESULT;
    unsafe fn _unused3(&self) -> HRESULT;
    unsafe fn _unused4(&self) -> HRESULT;
    unsafe fn _unused5(&self) -> HRESULT;
    unsafe fn _unused6(&self) -> HRESULT;
    unsafe fn _unused7(&self) -> HRESULT;
    unsafe fn _unused8(&self) -> HRESULT;
    unsafe fn _get_property_value_unused(&self) -> HRESULT;
    unsafe fn _set_property_value_unused(&self) -> HRESULT;
    unsafe fn set_default_endpoint(&self, device_id: PCWSTR, role: ERole) -> HRESULT;
    /// `visible` is `VARIANT_BOOL`-shaped (`i16`, not Win32 `BOOL`'s `i32`) —
    /// EarTrumpet marshals it as `short` (`[MarshalAs(UnmanagedType.I2)]`).
    unsafe fn set_endpoint_visibility(&self, device_id: PCWSTR, visible: i16) -> HRESULT;
}

/// These two interfaces are WinRT (`IInspectable`-derived, not raw `IUnknown`)
/// in the real ABI — but windows-core 0.62's `#[interface]` macro requires a
/// `{Parent}_Impl` trait for any non-`IUnknown` parent, and it doesn't
/// generate one for `IInspectable` itself (that trait only exists for
/// interfaces windows-rs's own codegen produces). Declaring `: IUnknown`
/// here and placeholder-slotting `IInspectable`'s own 3 methods
/// (`GetIids`/`GetRuntimeClassName`/`GetTrustLevel`) by hand produces the
/// identical vtable layout — what matters for ABI correctness is the actual
/// memory layout, not which Rust trait hierarchy describes it, and this
/// crate never calls any of the 3.
///
/// 19 unused placeholder slots after that (EarTrumpet's `__incomplete__...`
/// methods — undocumented volume-group/chat-app surface this module never
/// touches), then `GetPersistedDefaultAudioEndpoint`/
/// `ClearAllPersistedApplicationDefaultEndpoints` (also placeholder'd —
/// never called; `clear_route` uses `SetPersistedDefaultAudioEndpoint` with
/// a null device id, not the "clear all apps" method), then the one real
/// method this module calls. IID from EarTrumpet's
/// `IAudioPolicyConfigFactoryVariantFor21H2.cs` (post-21H2 Windows builds).
#[interface("ab3d4648-e242-459f-b02f-541c70306324")]
unsafe trait IAudioPolicyConfigFactory21H2: IUnknown {
    unsafe fn _get_iids_unused(&self) -> HRESULT;
    unsafe fn _get_runtime_class_name_unused(&self) -> HRESULT;
    unsafe fn _get_trust_level_unused(&self) -> HRESULT;
    unsafe fn _unused01(&self) -> HRESULT;
    unsafe fn _unused02(&self) -> HRESULT;
    unsafe fn _unused03(&self) -> HRESULT;
    unsafe fn _unused04(&self) -> HRESULT;
    unsafe fn _unused05(&self) -> HRESULT;
    unsafe fn _unused06(&self) -> HRESULT;
    unsafe fn _unused07(&self) -> HRESULT;
    unsafe fn _unused08(&self) -> HRESULT;
    unsafe fn _unused09(&self) -> HRESULT;
    unsafe fn _unused10(&self) -> HRESULT;
    unsafe fn _unused11(&self) -> HRESULT;
    unsafe fn _unused12(&self) -> HRESULT;
    unsafe fn _unused13(&self) -> HRESULT;
    unsafe fn _unused14(&self) -> HRESULT;
    unsafe fn _unused15(&self) -> HRESULT;
    unsafe fn _unused16(&self) -> HRESULT;
    unsafe fn _unused17(&self) -> HRESULT;
    unsafe fn _unused18(&self) -> HRESULT;
    unsafe fn _unused19(&self) -> HRESULT;
    unsafe fn _get_persisted_default_audio_endpoint_unused(&self) -> HRESULT;
    unsafe fn set_persisted_default_audio_endpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: PCWSTR,
    ) -> HRESULT;
    unsafe fn _clear_all_persisted_application_default_endpoints_unused(&self) -> HRESULT;
}

/// Same shape as [`IAudioPolicyConfigFactory21H2`] (see its doc comment for
/// why this extends `IUnknown` with 3 manually-placeholder'd `IInspectable`
/// slots rather than `IInspectable` directly), different IID (pre-21H2
/// Windows builds). IID from EarTrumpet's
/// `IAudioPolicyConfigFactoryVariantForDownlevel.cs`.
#[interface("2a59116d-6c4f-45e0-a74f-707e3fef9258")]
unsafe trait IAudioPolicyConfigFactoryDownlevel: IUnknown {
    unsafe fn _get_iids_unused(&self) -> HRESULT;
    unsafe fn _get_runtime_class_name_unused(&self) -> HRESULT;
    unsafe fn _get_trust_level_unused(&self) -> HRESULT;
    unsafe fn _unused01(&self) -> HRESULT;
    unsafe fn _unused02(&self) -> HRESULT;
    unsafe fn _unused03(&self) -> HRESULT;
    unsafe fn _unused04(&self) -> HRESULT;
    unsafe fn _unused05(&self) -> HRESULT;
    unsafe fn _unused06(&self) -> HRESULT;
    unsafe fn _unused07(&self) -> HRESULT;
    unsafe fn _unused08(&self) -> HRESULT;
    unsafe fn _unused09(&self) -> HRESULT;
    unsafe fn _unused10(&self) -> HRESULT;
    unsafe fn _unused11(&self) -> HRESULT;
    unsafe fn _unused12(&self) -> HRESULT;
    unsafe fn _unused13(&self) -> HRESULT;
    unsafe fn _unused14(&self) -> HRESULT;
    unsafe fn _unused15(&self) -> HRESULT;
    unsafe fn _unused16(&self) -> HRESULT;
    unsafe fn _unused17(&self) -> HRESULT;
    unsafe fn _unused18(&self) -> HRESULT;
    unsafe fn _unused19(&self) -> HRESULT;
    unsafe fn _get_persisted_default_audio_endpoint_unused(&self) -> HRESULT;
    unsafe fn set_persisted_default_audio_endpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: PCWSTR,
    ) -> HRESULT;
    unsafe fn _clear_all_persisted_application_default_endpoints_unused(&self) -> HRESULT;
}

enum AudioPolicyConfigFactory {
    Modern(IAudioPolicyConfigFactory21H2),
    Downlevel(IAudioPolicyConfigFactoryDownlevel),
}

impl AudioPolicyConfigFactory {
    unsafe fn set_persisted_default_audio_endpoint(
        &self,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: PCWSTR,
    ) -> HRESULT {
        match self {
            AudioPolicyConfigFactory::Modern(f) => unsafe {
                f.set_persisted_default_audio_endpoint(process_id, flow, role, device_id)
            },
            AudioPolicyConfigFactory::Downlevel(f) => unsafe {
                f.set_persisted_default_audio_endpoint(process_id, flow, role, device_id)
            },
        }
    }
}

/// Tries the post-21H2 IID first, falls back to the pre-21H2 one — same
/// fallback EarTrumpet/SoundSwitch use, done by trying both rather than
/// branching on an OS build number (no build-number threshold to keep in
/// sync with future Windows releases).
///
/// Uncertainty flagged for real-hardware verification: this calls
/// `com::ensure_initialized()` (classic COM MTA init) before activating,
/// matching every other COM entry point in this crate — but WinRT
/// activation may additionally require `RoInitialize` on the calling
/// thread. If activation reliably fails here on real hardware with an
/// apartment-related HRESULT, add a `RoInitializeWrapper`-style guard next
/// to `com::ComGuard`, joined the same lazy per-thread way.
fn activate_policy_config_factory() -> Result<AudioPolicyConfigFactory, PolicyError> {
    crate::com::ensure_initialized().map_err(|e| PolicyError::Unavailable(e.to_string()))?;
    let name = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
    unsafe {
        if let Ok(factory) = RoGetActivationFactory::<IAudioPolicyConfigFactory21H2>(&name) {
            return Ok(AudioPolicyConfigFactory::Modern(factory));
        }
        if let Ok(factory) = RoGetActivationFactory::<IAudioPolicyConfigFactoryDownlevel>(&name) {
            return Ok(AudioPolicyConfigFactory::Downlevel(factory));
        }
    }
    Err(PolicyError::Unavailable(
        "AudioPolicyConfig activation failed (tried both post-21H2 and pre-21H2 IIDs)".into(),
    ))
}

fn activate_policy_config_win7() -> Result<IPolicyConfigWin7, PolicyError> {
    crate::com::ensure_initialized().map_err(|e| PolicyError::Unavailable(e.to_string()))?;
    unsafe {
        CoCreateInstance(&POLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
            .map_err(|e| PolicyError::Unavailable(e.to_string()))
    }
}

/// Activation succeeded (the surface is usable) but this call didn't —
/// `PolicyError::Failed`, not `Unavailable` (see the type's own doc comment
/// in `engine::ports`).
fn hresult_to_policy_error(e: windows::core::Error) -> PolicyError {
    PolicyError::Failed(e.to_string())
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `PolicyPort` implementation. No fields: every method activates its COM
/// object fresh and drops it before returning, so there's no cross-call COM
/// object lifetime to manage here (unlike `WasapiSessions`'s persistent
/// notification registrations) — and no `unsafe impl Send` needed either,
/// since a zero-field struct is `Send` by construction.
pub struct PolicyRouter;

impl PolicyRouter {
    pub fn new() -> PolicyRouter {
        PolicyRouter
    }
}

impl Default for PolicyRouter {
    fn default() -> PolicyRouter {
        PolicyRouter::new()
    }
}

impl PolicyPort for PolicyRouter {
    fn route(&mut self, pid: u32, bus: &EndpointId) -> Result<(), PolicyError> {
        let factory = activate_policy_config_factory()?;
        let wide = to_wide(&bus.0);
        unsafe {
            factory.set_persisted_default_audio_endpoint(pid, eRender, eConsole, PCWSTR(wide.as_ptr()))
        }
        .ok()
        .map_err(hresult_to_policy_error)
    }

    /// `SetPersistedDefaultAudioEndpoint` with a null device id clears this
    /// pid's override — not `ClearAllPersistedApplicationDefaultEndpoints`,
    /// which would clear every app's override, not just this one.
    fn clear_route(&mut self, pid: u32) -> Result<(), PolicyError> {
        let factory = activate_policy_config_factory()?;
        unsafe { factory.set_persisted_default_audio_endpoint(pid, eRender, eConsole, PCWSTR::null()) }
            .ok()
            .map_err(hresult_to_policy_error)
    }

    fn set_visibility(&mut self, endpoint: &EndpointId, visible: bool) -> Result<(), PolicyError> {
        let policy = activate_policy_config_win7()?;
        let wide = to_wide(&endpoint.0);
        unsafe { policy.set_endpoint_visibility(PCWSTR(wide.as_ptr()), if visible { 1 } else { 0 }) }
            .ok()
            .map_err(hresult_to_policy_error)
    }

    /// `eConsole`, not `eMultimedia`/`eCommunications` — same role this
    /// crate's `default_output()`/device-change monitor already standardize
    /// on for "the default device".
    fn set_default(&mut self, endpoint: &EndpointId) -> Result<(), PolicyError> {
        let policy = activate_policy_config_win7()?;
        let wide = to_wide(&endpoint.0);
        unsafe { policy.set_default_endpoint(PCWSTR(wide.as_ptr()), eConsole) }
            .ok()
            .map_err(hresult_to_policy_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to real undocumented WASAPI/WinRT surfaces — not part of the
    /// normal suite (undocumented API, could break on a Windows update; no
    /// hardware/session guarantee in CI either). Run explicitly:
    /// `cargo test -p win-audio --features policy-routing -- --ignored
    /// hide_and_show_a_real_bus_endpoint`. Requires a real bus endpoint name
    /// — pass one via `SPLITSTREAM_TEST_BUS_ID` or this test is skipped.
    #[test]
    #[ignore]
    fn hide_and_show_a_real_bus_endpoint() {
        let Ok(id) = std::env::var("SPLITSTREAM_TEST_BUS_ID") else {
            println!("SPLITSTREAM_TEST_BUS_ID not set — skipping");
            return;
        };
        let endpoint = EndpointId(id);
        let mut router = PolicyRouter::new();
        router.set_visibility(&endpoint, false).expect("hide");
        println!("hid {endpoint:?} — check the Windows sound control panel");
        router.set_visibility(&endpoint, true).expect("show");
        println!("restored visibility for {endpoint:?}");
    }
}
