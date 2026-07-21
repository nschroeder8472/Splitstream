use std::sync::mpsc::Receiver;
use std::sync::Mutex;

use engine::ports::{
    AudioSystem, CapturePort, DeviceEvent, Endpoint, EndpointId, PortError, RenderPort, RtGuard,
};

use crate::enumerator::EndpointEnumerator;
use crate::monitor::DeviceMonitor;

pub struct WasapiSystem {
    enumerator: EndpointEnumerator,
    /// Holds the live device-change registration, if any. `Mutex` because
    /// `subscribe_device_events` takes `&self` (trait signature) but must
    /// replace whatever was previously registered — dropping the old
    /// `DeviceMonitor` unregisters it (drift-and-recovery L4 contract note:
    /// "a second call replaces the previous subscription").
    monitor: Mutex<Option<DeviceMonitor>>,
}

impl WasapiSystem {
    /// `bus_name_prefix`: render endpoints whose friendly name starts with
    /// this are classified as `Bus`, everything else `Physical` — see the
    /// classification note in `enumerator.rs`. Splitstream rides a bundled
    /// third-party virtual driver (VB-Audio / VB-CABLE), so bus detection is
    /// by configurable name prefix rather than a hardcoded vendor scheme.
    pub fn new(bus_name_prefix: impl Into<String>) -> WasapiSystem {
        WasapiSystem {
            enumerator: EndpointEnumerator::new(bus_name_prefix),
            monitor: Mutex::new(None),
        }
    }
}

impl AudioSystem for WasapiSystem {
    fn enumerate(&self) -> Result<Vec<Endpoint>, PortError> {
        self.enumerator.enumerate()
    }

    fn open_capture(&self, id: &EndpointId) -> Result<Box<dyn CapturePort>, PortError> {
        Ok(Box::new(crate::capture::open(id)?))
    }

    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError> {
        Ok(Box::new(crate::render::open(id)?))
    }

    fn promote_rt_thread(&self) -> RtGuard {
        crate::mmcss::promote_current_thread()
    }

    fn default_output(&self) -> Result<Endpoint, PortError> {
        self.enumerator.default_output()
    }

    fn subscribe_device_events(&self) -> Result<Receiver<DeviceEvent>, PortError> {
        let (monitor, rx) = crate::monitor::subscribe(self.enumerator.bus_name_prefix())?;
        *self.monitor.lock().unwrap() = Some(monitor);
        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engine::ports::EndpointKind;
    use std::time::Duration;

    /// Opens real WASAPI capture + render against whatever physical device
    /// this machine has, reads/writes a few cycles, drops cleanly. Render
    /// only ever writes silence — no audible output. Not part of the normal
    /// suite (no audio hardware guarantee in CI). Run explicitly:
    /// `cargo test -p win-audio -- --ignored open_and_pump_a_real_device`.
    #[test]
    #[ignore]
    fn open_and_pump_a_real_device() {
        let sys = WasapiSystem::new("Splitstream Bus");
        let endpoints = sys.enumerate().expect("enumerate");
        let physical = endpoints
            .iter()
            .find(|e| e.kind == EndpointKind::Physical)
            .expect("expected at least one physical render endpoint");

        let mut render = sys.open_render(&physical.id).expect("open_render");
        let mut capture = sys
            .open_capture(&physical.id)
            .expect("open_capture (loopback)");

        let channels = render.format().channels as usize;
        let silence = vec![0.0f32; render.period_frames() * channels];
        for _ in 0..5 {
            render
                .wait_event(Duration::from_millis(200))
                .expect("wait_event");
            render.write(&silence).expect("write");
        }

        let mut buf = vec![0.0f32; 4096];
        let n = capture.read(&mut buf).expect("read");
        println!("loopback captured {n} samples in one read()");

        let _guard = sys.promote_rt_thread();
    }

    /// Real `GetDefaultAudioEndpoint(eRender, eConsole)` on whatever machine
    /// runs it. Not part of the normal suite (no audio hardware guarantee in
    /// CI). Run explicitly:
    /// `cargo test -p win-audio -- --ignored default_output_returns_a_real_physical_endpoint`.
    #[test]
    #[ignore]
    fn default_output_returns_a_real_physical_endpoint() {
        let sys = WasapiSystem::new("Splitstream Bus");
        let default = sys.default_output().expect("default_output");
        assert_eq!(default.kind, EndpointKind::Physical);
        println!("default output: {default:?}");
    }

    /// Registers a real `IMMNotificationClient` and prints whatever events
    /// arrive — verifying automatically requires physically plugging/
    /// unplugging a device, so this is a manual smoke test, not an
    /// assertion. Not part of the normal suite. Run explicitly:
    /// `cargo test -p win-audio -- --ignored --nocapture subscribe_and_print_real_device_events`,
    /// then plug/unplug a device within the sleep window.
    #[test]
    #[ignore]
    fn subscribe_and_print_real_device_events() {
        let sys = WasapiSystem::new("Splitstream Bus");
        let rx = sys.subscribe_device_events().expect("subscribe_device_events");
        println!("listening for device events for 10s — plug/unplug a device now");
        while let Ok(evt) = rx.recv_timeout(Duration::from_secs(10)) {
            println!("{evt:?}");
        }
    }
}
