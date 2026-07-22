//! `IMMNotificationClient` → typed `DeviceEvent` channel (drift-and-recovery
//! L4 decision: `AudioSystem::subscribe_device_events`, no second port
//! trait). Registration lifetime is owned by [`DeviceMonitor`] — dropping it
//! unregisters the callback, so `WasapiSystem` holding at most one
//! `DeviceMonitor` at a time gives "a second `subscribe_device_events` call
//! replaces the previous subscription" (the documented contract) for free.

use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;

use windows::core::{implement, PCWSTR};
use windows::Win32::Foundation::PROPERTYKEY;
use windows::Win32::Media::Audio::{
    eConsole, eRender, EDataFlow, ERole, IMMDeviceEnumerator, IMMNotificationClient,
    IMMNotificationClient_Impl, MMDeviceEnumerator, DEVICE_STATE,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use engine::ports::{DeviceEvent, EndpointId, PortError};

use crate::enumerator::describe_device_by_id;

#[implement(IMMNotificationClient)]
struct NotificationSink {
    tx: Sender<DeviceEvent>,
}

impl IMMNotificationClient_Impl for NotificationSink_Impl {
    fn OnDeviceStateChanged(
        &self,
        device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        let _ = self
            .tx
            .send(DeviceEvent::StateChanged(EndpointId(pwstr_to_string(device_id))));
        Ok(())
    }

    fn OnDeviceAdded(&self, device_id: &PCWSTR) -> windows::core::Result<()> {
        // `describe_device_by_id` calls back into `IMMDeviceEnumerator::GetDevice`
        // (via `device::open`) — MSDN's IMMNotificationClient remarks warn that
        // calling back into the enumerator's GetDevice/EnumAudioEndpoints/
        // GetDefaultAudioEndpoint *synchronously from inside a notification
        // callback* can deadlock the OS's shared notification thread. Describe
        // on a short-lived worker thread instead of the callback thread — the
        // callback itself only needs to return promptly. Best-effort: a device
        // that fails to describe (e.g. no render format available yet) is
        // silently skipped rather than surfaced as a malformed event —
        // `enumerate()` will pick it up once it settles.
        let id = EndpointId(pwstr_to_string(device_id));
        let tx = self.tx.clone();
        thread::spawn(move || {
            if let Ok(Some(endpoint)) = describe_device_by_id(&id) {
                let _ = tx.send(DeviceEvent::Added(endpoint));
            }
        });
        Ok(())
    }

    fn OnDeviceRemoved(&self, device_id: &PCWSTR) -> windows::core::Result<()> {
        let _ = self
            .tx
            .send(DeviceEvent::Removed(EndpointId(pwstr_to_string(device_id))));
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        default_device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        // Scoped to the console role on the render flow — the same pair
        // `EndpointEnumerator::default_output` queries. Capture-flow and
        // communications/multimedia-role changes are outside this feature's
        // scope (drift-and-recovery is output-device recovery only).
        if flow == eRender && role == eConsole {
            let _ = self.tx.send(DeviceEvent::DefaultChanged(EndpointId(
                pwstr_to_string(default_device_id),
            )));
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(()) // no property change the engine currently acts on
    }
}

fn pwstr_to_string(s: &PCWSTR) -> String {
    unsafe { s.to_string().unwrap_or_default() }
}

/// Owns the live registration; unregisters on drop.
pub struct DeviceMonitor {
    enumerator: IMMDeviceEnumerator,
    client: IMMNotificationClient,
}

// SAFETY: same reasoning as `WasapiRender`'s identical note — every thread
// that touches COM in this crate joins the MTA via `com::ensure_initialized`
// first, so an MTA object (both interfaces here are `CoCreateInstance`'d
// under `CLSCTX_ALL` on an MTA thread) is safe to use from any other MTA
// thread, not just the one that created it. `WasapiSystem` only ever
// accesses `DeviceMonitor` behind a `Mutex`, so no concurrent access to
// worry about beyond the cross-thread move itself.
unsafe impl Send for DeviceMonitor {}

impl Drop for DeviceMonitor {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .enumerator
                .UnregisterEndpointNotificationCallback(&self.client);
        }
    }
}

pub fn subscribe() -> Result<(DeviceMonitor, Receiver<DeviceEvent>), PortError> {
    crate::com::ensure_initialized().map_err(|e| PortError::Backend(e.to_string()))?;
    let (tx, rx) = mpsc::channel();
    unsafe {
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        let sink = NotificationSink { tx };
        let client: IMMNotificationClient = sink.into();
        enumerator
            .RegisterEndpointNotificationCallback(&client)
            .map_err(|e| PortError::Backend(e.to_string()))?;
        Ok((DeviceMonitor { enumerator, client }, rx))
    }
}
