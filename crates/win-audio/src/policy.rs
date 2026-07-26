//! `IPolicyConfig` — setting the Windows default render endpoint
//! (double-audio-prevention L4). The one undocumented COM surface in this
//! design: Windows exposes no supported way to change the default playback
//! device, and capability 4 ("a clean exit restores the previous default")
//! cannot be met without it.
//!
//! Both GUIDs and the full vtable order below were re-derived 2026-07-26 from
//! EarTrumpet's own source (`Interop/MMDeviceAPI/IPolicyConfig.cs` and
//! `PolicyConfigClient.cs`), not from this repo's `implementation-notes.md`
//! sketch — that sketch declares **2** methods where the real interface has
//! **12** own vtable slots, and a skipped slot shifts every later method,
//! which is memory corruption rather than an error code. The note is corrected
//! in the same change that adds this file.

use windows::core::{interface, IUnknown, IUnknown_Vtbl, GUID, HRESULT, PCWSTR};
use windows::Win32::Media::Audio::{eCommunications, eConsole, eMultimedia};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use engine::ports::PortError;

/// `IPolicyConfig` (Win7+). IID `F8679F50-850A-41CF-9C72-430F290290C8`.
///
/// Slots 1-8 are unused in every reference implementation but still occupy
/// vtable entries, so they must be declared. Only `set_default_endpoint`
/// (slot 11) is ever called; `_set_endpoint_visibility` (slot 12) is declared
/// solely so the layout is complete and reviewable against the reference.
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
    unsafe fn _get_property_value(&self) -> HRESULT;
    unsafe fn _set_property_value(&self) -> HRESULT;
    unsafe fn set_default_endpoint(&self, device_id: PCWSTR, role: i32) -> HRESULT;
    unsafe fn _set_endpoint_visibility(&self) -> HRESULT;
}

/// `CPolicyConfigClient`, the coclass exposing [`IPolicyConfigWin7`].
const CPOLICY_CONFIG_CLIENT: GUID = GUID::from_u128(0x870af99c_171d_4f9e_af0d_e63df40c2bc9);

/// Makes `device_id` the default render endpoint for all three roles.
///
/// All three, never just `eConsole`: Windows resolves `eCommunications`
/// separately, and leaving it behind would let Discord-class apps keep
/// rendering to the real device and double there — the exact bug this
/// feature removes.
///
/// Every role is attempted even if an earlier one fails, so a single bad
/// HRESULT can't silently skip the remaining roles; the first error is
/// returned once all three have been tried. Never panics, never retries —
/// the caller surfaces the failure (capability 6) and leaves it at that.
pub fn set_default_endpoint_all_roles(device_id: &str) -> Result<(), PortError> {
    crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
    let wide: Vec<u16> = device_id.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let config: IPolicyConfigWin7 = CoCreateInstance(&CPOLICY_CONFIG_CLIENT, None, CLSCTX_ALL)
            .map_err(|e| PortError::Backend(format!("IPolicyConfig unavailable: {e}")))?;

        let mut first_error = None;
        for role in [eConsole, eMultimedia, eCommunications] {
            let hr = config.set_default_endpoint(PCWSTR(wide.as_ptr()), role.0);
            if hr.is_err() && first_error.is_none() {
                first_error = Some(PortError::Backend(format!(
                    "SetDefaultEndpoint(role {}) failed: {hr}",
                    role.0
                )));
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The corruption class this file exists to avoid: if `SetDefaultEndpoint`
    /// were declared at any slot other than 11, this call would invoke a
    /// different method entirely — with the wrong argument types, and no error
    /// code to say so. Only real COM can tell us; a wrong slot typically
    /// crashes or returns a nonsense HRESULT rather than moving the default.
    ///
    /// Changes the machine's default playback device, so it is opt-in and
    /// restores what it found. Run explicitly:
    /// `cargo test -p win-audio -- --ignored --nocapture set_default_endpoint_is_vtable_slot_11`.
    #[test]
    #[ignore]
    fn set_default_endpoint_is_vtable_slot_11() {
        let enumerator = crate::enumerator::EndpointEnumerator::new();
        let original = enumerator.default_output().expect("default_output");
        let endpoints = enumerator.enumerate().expect("enumerate");
        let other = endpoints
            .iter()
            .find(|e| e.id != original.id)
            .expect("need at least two render endpoints to prove the default actually moved");

        println!("original default: {:?}", original.name);
        set_default_endpoint_all_roles(&other.id.0).expect("set_default_endpoint_all_roles");

        let moved = enumerator.default_output().expect("default_output after set");
        set_default_endpoint_all_roles(&original.id.0).expect("restore original default");

        assert_eq!(
            moved.id, other.id,
            "slot 11 must be SetDefaultEndpoint — the default did not move"
        );
    }
}
