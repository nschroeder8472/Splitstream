//! Fake `AudioSystem` + ports. This is why the port traits live in `engine`,
//! not `win-audio`: the whole graph runs on any platform against these fakes.

use std::f32::consts::TAU;
use std::time::Duration;

use audio_core::Format;

use super::{AudioSystem, CapturePort, Endpoint, EndpointId, PortError, RenderPort, RtGuard};

pub struct MockSystem {
    endpoints: Vec<Endpoint>,
}

impl MockSystem {
    pub fn new(endpoints: Vec<Endpoint>) -> MockSystem {
        MockSystem { endpoints }
    }

    fn find(&self, id: &EndpointId) -> Result<&Endpoint, PortError> {
        self.endpoints
            .iter()
            .find(|e| &e.id == id)
            .ok_or_else(|| PortError::NotFound(id.clone()))
    }
}

impl AudioSystem for MockSystem {
    fn enumerate(&self) -> Result<Vec<Endpoint>, PortError> {
        Ok(self.endpoints.clone())
    }

    fn open_capture(&self, id: &EndpointId) -> Result<Box<dyn CapturePort>, PortError> {
        let endpoint = self.find(id)?;
        Ok(Box::new(SineCapture::new(440.0, endpoint.format)))
    }

    fn open_render(&self, id: &EndpointId) -> Result<Box<dyn RenderPort>, PortError> {
        let endpoint = self.find(id)?;
        Ok(Box::new(SinkRender::new(endpoint.format)))
    }

    fn promote_rt_thread(&self) -> RtGuard {
        RtGuard::noop()
    }
}

/// Deterministic signal source: a sine wave at `freq_hz`, same value on every
/// channel. Lets tests assert on exact expected samples instead of "some audio".
pub struct SineCapture {
    format: Format,
    freq_hz: f32,
    phase: f32,
}

impl SineCapture {
    pub fn new(freq_hz: f32, format: Format) -> SineCapture {
        SineCapture {
            format,
            freq_hz,
            phase: 0.0,
        }
    }
}

impl CapturePort for SineCapture {
    fn read(&mut self, buf: &mut [f32]) -> Result<usize, PortError> {
        let channels = self.format.channels as usize;
        let step = TAU * self.freq_hz / self.format.sample_rate as f32;
        for frame in buf.chunks_exact_mut(channels.max(1)) {
            let sample = self.phase.sin();
            frame.fill(sample);
            self.phase = (self.phase + step) % TAU;
        }
        Ok(buf.len())
    }

    fn format(&self) -> Format {
        self.format
    }

    fn poll_interval(&self) -> Duration {
        Duration::from_millis(5)
    }
}

/// Records every frame written to it, for test assertions (gain applied?
/// groups summed correctly?). Never returns an error or blocks `wait_event`.
pub struct SinkRender {
    format: Format,
    recorded: Vec<f32>,
}

impl SinkRender {
    pub fn new(format: Format) -> SinkRender {
        SinkRender {
            format,
            recorded: Vec::new(),
        }
    }

    pub fn recorded(&self) -> &[f32] {
        &self.recorded
    }
}

impl RenderPort for SinkRender {
    fn wait_event(&mut self, _timeout: Duration) -> Result<(), PortError> {
        Ok(())
    }

    fn write(&mut self, frames: &[f32]) -> Result<(), PortError> {
        self.recorded.extend_from_slice(frames);
        Ok(())
    }

    fn format(&self) -> Format {
        self.format
    }

    fn period_frames(&self) -> usize {
        480
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ports::EndpointKind;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: audio_core::ChannelLayout::STEREO,
        }
    }

    fn endpoint(id: &str, kind: EndpointKind) -> Endpoint {
        Endpoint {
            id: EndpointId(id.to_string()),
            name: id.to_string(),
            kind,
            format: stereo(48_000),
        }
    }

    #[test]
    fn enumerate_returns_configured_endpoints() {
        let sys = MockSystem::new(vec![
            endpoint("bus-1", EndpointKind::Bus),
            endpoint("out-1", EndpointKind::Physical),
        ]);
        let eps = sys.enumerate().unwrap();
        assert_eq!(eps.len(), 2);
    }

    #[test]
    fn open_capture_on_unknown_id_returns_not_found() {
        let sys = MockSystem::new(vec![]);
        let result = sys.open_capture(&EndpointId("missing".into()));
        assert!(matches!(result, Err(PortError::NotFound(_))));
    }

    #[test]
    fn sine_capture_fills_buffer_and_is_deterministic_across_captures() {
        let fmt = stereo(48_000);
        let mut a = SineCapture::new(440.0, fmt);
        let mut b = SineCapture::new(440.0, fmt);
        let mut buf_a = [0.0f32; 8];
        let mut buf_b = [0.0f32; 8];
        assert_eq!(a.read(&mut buf_a).unwrap(), 8);
        assert_eq!(b.read(&mut buf_b).unwrap(), 8);
        assert_eq!(buf_a, buf_b);
        // Both channels of a frame carry the same sample.
        assert_eq!(buf_a[0], buf_a[1]);
    }

    #[test]
    fn sink_render_records_every_write() {
        let mut sink = SinkRender::new(stereo(48_000));
        sink.write(&[0.1, 0.2]).unwrap();
        sink.write(&[0.3, 0.4]).unwrap();
        assert_eq!(sink.recorded(), &[0.1, 0.2, 0.3, 0.4]);
    }
}
