//! One-pole parameter smoother shared by `mixer.rs` and `dsp.rs` — every
//! audible parameter ramps toward its target instead of stepping (a stepped
//! value is an audible "zipper" click, notes §8).

#[derive(Debug, Clone, Copy)]
pub(crate) struct Smoothed {
    current: f32,
    target: f32,
    coeff: f32,
}

impl Smoothed {
    pub(crate) fn new(initial: f32, sample_rate: u32, time_constant_s: f32) -> Smoothed {
        let coeff = (-1.0 / (time_constant_s * sample_rate as f32)).exp();
        Smoothed {
            current: initial,
            target: initial,
            coeff,
        }
    }

    pub(crate) fn set_target(&mut self, target: f32) {
        self.target = target;
    }

    #[inline]
    pub(crate) fn next(&mut self) -> f32 {
        self.current = self.target + self.coeff * (self.current - self.target);
        self.current
    }
}
