//! `IMMDeviceEnumerator` → discover physical render endpoints (spec F1).
//!
//! No more Bus/Physical classification (process-loopback-capture pivot):
//! every active render endpoint is a plain output-device candidate now —
//! there is no virtual bus to distinguish, since capture is per-process, not
//! per-endpoint.

use windows::core::PWSTR;
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator, DEVICE_STATE_ACTIVE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL, STGM_READ};

use engine::ports::{Endpoint, EndpointId, PortError};

use crate::format::client_mix_format;

pub struct EndpointEnumerator;

impl EndpointEnumerator {
    pub fn new() -> EndpointEnumerator {
        EndpointEnumerator
    }

    pub fn enumerate(&self) -> Result<Vec<Endpoint>, PortError> {
        crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| PortError::Backend(e.to_string()))?;

            let collection = enumerator
                .EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
                .map_err(|e| PortError::Backend(e.to_string()))?;
            let count = collection
                .GetCount()
                .map_err(|e| PortError::Backend(e.to_string()))?;

            let mut endpoints = Vec::with_capacity(count as usize);
            for i in 0..count {
                let device = collection
                    .Item(i)
                    .map_err(|e| PortError::Backend(e.to_string()))?;
                if let Some(endpoint) = describe_device(&device)? {
                    endpoints.push(endpoint);
                }
            }

            Ok(endpoints)
        }
    }

    /// `default_output()` port method (drift-and-recovery L4): the recovery
    /// supervisor's fallback target on device removal, and
    /// process-loopback-capture's `capture_format` source (every group's
    /// `input_format` — `graph::resolve`'s doc). `eConsole`, not
    /// `eMultimedia`/`eCommunications` — matches what most apps (and the
    /// user's system volume mixer) treat as "the default device".
    pub fn default_output(&self) -> Result<Endpoint, PortError> {
        crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| PortError::Backend(e.to_string()))?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .map_err(|e| PortError::Backend(e.to_string()))?;
            describe_device(&device)?.ok_or_else(|| {
                PortError::Backend("default render endpoint has no usable mix format".into())
            })
        }
    }
}

impl Default for EndpointEnumerator {
    fn default() -> EndpointEnumerator {
        EndpointEnumerator::new()
    }
}

/// Opens and describes one endpoint by id — used by the device-change
/// monitor (`monitor.rs`) to turn the bare device-id string an
/// `IMMNotificationClient` callback hands us into a full `Endpoint` for
/// `DeviceEvent::Added`.
pub(crate) fn describe_device_by_id(id: &EndpointId) -> Result<Option<Endpoint>, PortError> {
    crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
    let device = crate::device::open(id)?;
    unsafe { describe_device(&device) }
}

/// Reads id/name/format for one device. Returns `Ok(None)` when the format
/// can't be determined — skip that one endpoint rather than failing the
/// whole enumeration.
unsafe fn describe_device(device: &IMMDevice) -> Result<Option<Endpoint>, PortError> {
    let id = device
        .GetId()
        .map_err(|e| PortError::Backend(e.to_string()))?;
    let id = pwstr_to_string(id);

    let store = device
        .OpenPropertyStore(STGM_READ)
        .map_err(|e| PortError::Backend(e.to_string()))?;
    let mut name_prop = store
        .GetValue(&PKEY_Device_FriendlyName)
        .map_err(|e| PortError::Backend(e.to_string()))?;
    let name = name_prop.to_string();
    let _ = windows::Win32::System::Com::StructuredStorage::PropVariantClear(&mut name_prop);

    let format = match client_mix_format(device) {
        Ok(f) => f,
        Err(_) => return Ok(None),
    };

    Ok(Some(Endpoint {
        id: EndpointId(id),
        name,
        format,
    }))
}

unsafe fn pwstr_to_string(s: PWSTR) -> String {
    let result = s.to_string().unwrap_or_default();
    windows::Win32::System::Com::CoTaskMemFree(Some(s.0 as *const _));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Talks to real WASAPI on whatever machine runs it — not part of the
    /// normal suite (no audio hardware guarantee in CI). Run explicitly:
    /// `cargo test -p win-audio -- --ignored enumerate_real_render_endpoints`.
    #[test]
    #[ignore]
    fn enumerate_real_render_endpoints() {
        let endpoints = EndpointEnumerator::new()
            .enumerate()
            .expect("enumerate should succeed on a machine with any render device");
        for e in &endpoints {
            println!("{:?} format={:?} name={:?}", e.id, e.format, e.name);
        }
        assert!(
            !endpoints.is_empty(),
            "expected at least one render endpoint"
        );
    }
}
