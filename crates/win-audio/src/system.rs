use engine::ports::{
    AudioSystem, CapturePort, Endpoint, EndpointId, PortError, RenderPort, RtGuard,
};

use crate::enumerator::EndpointEnumerator;

pub struct WasapiSystem {
    enumerator: EndpointEnumerator,
}

impl WasapiSystem {
    /// `bus_name_prefix`: render endpoints whose friendly name starts with
    /// this are classified as `Bus`, everything else `Physical` — see the
    /// classification note in `enumerator.rs` (bundled virtual driver choice
    /// is still an open question, so this isn't hardcoded to one vendor).
    pub fn new(bus_name_prefix: impl Into<String>) -> WasapiSystem {
        WasapiSystem {
            enumerator: EndpointEnumerator::new(bus_name_prefix),
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
}
