//! Shared "open an `IMMDevice` by id" helper for capture/render/format probing.

use windows::core::PCWSTR;
use windows::Win32::Media::Audio::{IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use engine::ports::{EndpointId, PortError};

pub fn open(id: &EndpointId) -> Result<IMMDevice, PortError> {
    crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| PortError::Backend(e.to_string()))?;
        let wide: Vec<u16> = id.0.encode_utf16().chain(std::iter::once(0)).collect();
        enumerator
            .GetDevice(PCWSTR(wide.as_ptr()))
            .map_err(|e| PortError::Backend(e.to_string()))
    }
}
