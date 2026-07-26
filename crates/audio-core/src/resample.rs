//! Sample rate conversion with continuous drift correction. Every `Src`
//! is a variable-ratio resampler from construction — even when `from` and
//! `to` share a nominal sample rate, real device clocks still drift, and
//! [`Src::set_ratio`] runs on the RT mixer thread (via `Mixer::apply`), so
//! there's no room to lazily swap in a resampler later.

use crate::sample::{DomainError, Format, ResampleRatio};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

/// Tuned down from rubato's library-suggested starting points (256/128):
/// the drift loop only ever asks for a ±0.5% trim (`DriftConfig::max_correction`),
/// which doesn't need a 256-tap/128x-oversampled sinc. Benchmarked at the real
/// `max_block_frames` block size — 64/32 costs ~0.1% of one core per group vs.
/// 0.285% at 256/128, with quality headroom to spare for a ±0.5% correction.
const SINC_LEN: usize = 64;
const F_CUTOFF: f32 = 0.95;
const OVERSAMPLING_FACTOR: usize = 32;
/// Per-chunk one-pole glide rate toward the target ratio — never step
/// (notes §11); ~20 chunks to settle, inaudible at typical block rates.
const RATIO_GLIDE_RATE: f64 = 0.05;

/// Progress of one [`Src::process`] call, in samples (interleaved elements,
/// i.e. `frames * channels`) — matches the unit callers already index `input`/`output` in.
pub struct SrcProgress {
    pub consumed: usize,
    pub produced: usize,
}

pub struct Src {
    resampler: SincFixedIn<f32>,
    channels: usize,
    chunk_in: usize,
    in_deint: Vec<Vec<f32>>,
    out_deint: Vec<Vec<f32>>,
    /// Interleaved leftover input not yet forming a full `chunk_in` — capacity `chunk_in * channels`.
    pending_in: Vec<f32>,
    pending_in_frames: usize,
    /// Interleaved output produced but not yet delivered to a caller — capacity
    /// `output_frames_max() * channels`. Only ever holds at most one chunk: the
    /// process loop never resamples another chunk until this one is fully drained.
    pending_out: Vec<f32>,
    pending_out_frames: usize,
    pending_out_read: usize,
    /// Relative-to-construction ratio, glided toward `target_ratio` one chunk at a time.
    current_ratio: f64,
    target_ratio: f64,
}

impl Src {
    pub fn new(from: Format, to: Format, max_block_frames: usize) -> Result<Src, DomainError> {
        if from.channels != to.channels {
            return Err(DomainError::ChannelMismatch {
                from: from.channels,
                to: to.channels,
            });
        }

        let channels = from.channels as usize;
        let base_ratio = to.sample_rate as f64 / from.sample_rate as f64;
        let chunk_size = max_block_frames.max(1);

        let params = SincInterpolationParameters {
            sinc_len: SINC_LEN,
            f_cutoff: F_CUTOFF,
            oversampling_factor: OVERSAMPLING_FACTOR,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };

        // `max_resample_ratio_relative` clamps the *reciprocal* too — rubato accepts
        // exactly [1/max, max]. Use whichever bound is wider than our own [MIN, MAX]
        // clamp, so every `ResampleRatio` we construct is guaranteed accepted here.
        let max_relative =
            crate::sample::MAX_RESAMPLE_RATIO.max(1.0 / crate::sample::MIN_RESAMPLE_RATIO);
        let resampler =
            SincFixedIn::<f32>::new(base_ratio, max_relative, params, chunk_size, channels)
                .map_err(|e| DomainError::ResamplerInit(e.to_string()))?;

        let chunk_in = resampler.input_frames_next();
        let chunk_out_max = resampler.output_frames_max();

        Ok(Src {
            in_deint: vec![vec![0.0f32; chunk_in]; channels],
            out_deint: vec![vec![0.0f32; chunk_out_max]; channels],
            pending_in: vec![0.0f32; chunk_in * channels],
            pending_in_frames: 0,
            pending_out: vec![0.0f32; chunk_out_max * channels],
            pending_out_frames: 0,
            pending_out_read: 0,
            resampler,
            channels,
            chunk_in,
            current_ratio: 1.0,
            target_ratio: 1.0,
        })
    }

    /// Sets the target *relative* ratio (relative to the base ratio fixed at
    /// construction — `1.0` means "no correction"). The actual ratio glides
    /// toward this one chunk at a time; never steps.
    pub fn set_ratio(&mut self, target: ResampleRatio) {
        self.target_ratio = target.value();
    }

    /// RT-safe: preallocated at construction, no allocation on this path.
    /// `input`/`output` must each hold a whole number of frames (`len % channels == 0`).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> SrcProgress {
        debug_assert_eq!(input.len() % self.channels, 0);
        debug_assert_eq!(output.len() % self.channels, 0);

        let in_frames_total = input.len() / self.channels;
        let out_frames_total = output.len() / self.channels;

        let mut consumed_frames = 0usize;
        // Deliver any output left over from a previous call before making more.
        let mut produced_frames = self.drain_pending_out(output);

        while produced_frames < out_frames_total {
            consumed_frames += self.top_up_pending_in(
                &input[consumed_frames * self.channels..in_frames_total * self.channels],
            );

            if self.pending_in_frames < self.chunk_in {
                break; // not enough input left to fill a chunk this call
            }

            // Invariant: we only get here once `pending_out` has been fully
            // drained above (loop guard), so overwriting it is safe.
            self.resample_pending_chunk();

            produced_frames +=
                self.drain_pending_out(&mut output[produced_frames * self.channels..]);
        }

        SrcProgress {
            consumed: consumed_frames * self.channels,
            produced: produced_frames * self.channels,
        }
    }

    /// Copies as much of `input` as fits into `pending_in`. Returns frames consumed.
    fn top_up_pending_in(&mut self, input: &[f32]) -> usize {
        let room = self.chunk_in - self.pending_in_frames;
        let take = room.min(input.len() / self.channels);
        if take > 0 {
            let dst = self.pending_in_frames * self.channels;
            self.pending_in[dst..dst + take * self.channels]
                .copy_from_slice(&input[..take * self.channels]);
            self.pending_in_frames += take;
        }
        take
    }

    /// Glides the ratio one step toward its target, deinterleaves one full
    /// `pending_in` chunk, resamples it, and interleaves the result into
    /// `pending_out`. Caller must ensure `pending_in_frames == chunk_in`.
    fn resample_pending_chunk(&mut self) {
        self.current_ratio += RATIO_GLIDE_RATE * (self.target_ratio - self.current_ratio);
        self.resampler
            .set_resample_ratio_relative(self.current_ratio, true)
            .expect("ratio within the max_resample_ratio_relative clamp set at construction");

        for (ch, chan_buf) in self.in_deint.iter_mut().enumerate() {
            for (i, sample) in chan_buf.iter_mut().enumerate() {
                *sample = self.pending_in[i * self.channels + ch];
            }
        }
        self.pending_in_frames = 0;

        let (_, out_len) = self
            .resampler
            .process_into_buffer(&self.in_deint, &mut self.out_deint, None)
            .expect("fixed-input-chunk resample with correctly sized preallocated buffers");

        for (ch, chan_buf) in self.out_deint.iter().enumerate() {
            for (i, sample) in chan_buf.iter().enumerate().take(out_len) {
                self.pending_out[i * self.channels + ch] = *sample;
            }
        }
        self.pending_out_frames = out_len;
        self.pending_out_read = 0;
    }

    fn drain_pending_out(&mut self, dst: &mut [f32]) -> usize {
        let avail = self.pending_out_frames - self.pending_out_read;
        let dst_frames = dst.len() / self.channels;
        let n = avail.min(dst_frames);
        if n > 0 {
            let src = self.pending_out_read * self.channels;
            dst[..n * self.channels]
                .copy_from_slice(&self.pending_out[src..src + n * self.channels]);
            self.pending_out_read += n;
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stereo(rate: u32) -> Format {
        Format {
            sample_rate: rate,
            channels: 2,
            layout: crate::sample::ChannelLayout::STEREO,
        }
    }

    fn feed_silence_for(
        src: &mut Src,
        block_frames: usize,
        channels: usize,
        blocks: usize,
    ) -> usize {
        let input = vec![0.0f32; block_frames * channels];
        let mut output = vec![0.0f32; block_frames * channels * 4];
        let mut total_produced = 0;
        for _ in 0..blocks {
            let progress = src.process(&input, &mut output);
            assert!(progress.consumed <= input.len());
            assert!(progress.produced <= output.len());
            total_produced += progress.produced;
        }
        total_produced
    }

    #[test]
    fn channel_mismatch_is_rejected() {
        // Src's own invariant — no longer reachable from Mixer (the channel
        // matrix guarantees equal channel counts upstream), but Src itself
        // still must not silently misinterpret mismatched buffers.
        let from = stereo(48_000);
        let to = Format {
            sample_rate: 48_000,
            channels: 1,
            layout: crate::sample::ChannelLayout::MONO,
        };
        assert!(matches!(
            Src::new(from, to, 256),
            Err(DomainError::ChannelMismatch { from: 2, to: 1 })
        ));
    }

    #[test]
    fn silence_in_produces_silence_out_at_unity_ratio() {
        let fmt = stereo(48_000);
        let mut src = Src::new(fmt, fmt, 512).unwrap();
        let input = vec![0.0f32; 512 * 2];
        let mut output = vec![1.0f32; 512 * 2 * 4]; // pre-fill non-zero to prove it gets overwritten
        let mut any_produced = false;
        for _ in 0..8 {
            let progress = src.process(&input, &mut output);
            if progress.produced > 0 {
                any_produced = true;
                assert!(output[..progress.produced].iter().all(|&s| s == 0.0));
            }
        }
        assert!(
            any_produced,
            "resampler never produced output across 8 chunks"
        );
    }

    #[test]
    fn never_overruns_caller_buffers_across_a_sample_rate_mismatch() {
        let from = stereo(44_100);
        let to = stereo(48_000);
        let mut src = Src::new(from, to, 512).unwrap();
        feed_silence_for(&mut src, 512, 2, 8);
    }

    #[test]
    fn set_ratio_above_unity_eventually_produces_more_output_than_input() {
        // ratio > 1.0 means the resampler runs faster than its base rate —
        // over enough blocks it must emit more frames than it consumed.
        let fmt = stereo(48_000);
        let mut src = Src::new(fmt, fmt, 256).unwrap();
        src.set_ratio(ResampleRatio::new(1.1).unwrap());

        let input = vec![0.0f32; 256 * 2];
        let mut output = vec![0.0f32; 256 * 2 * 4];
        let mut total_consumed = 0usize;
        let mut total_produced = 0usize;
        for _ in 0..40 {
            let progress = src.process(&input, &mut output);
            total_consumed += progress.consumed;
            total_produced += progress.produced;
        }

        assert!(
            total_produced > total_consumed,
            "expected ratio > 1.0 to produce more samples than consumed after glide settles \
             (consumed {total_consumed}, produced {total_produced})"
        );
    }

    #[test]
    fn set_ratio_never_steps_the_resampler_ratio_directly() {
        // Jump straight to the ratio clamp extreme; confirm the resampler
        // never receives more than one glide step's worth of change per chunk.
        let fmt = stereo(48_000);
        let mut src = Src::new(fmt, fmt, 256).unwrap();
        src.set_ratio(ResampleRatio::new(crate::sample::MAX_RESAMPLE_RATIO).unwrap());

        feed_silence_for(&mut src, 256, 2, 4);
        // First few chunks after a large target jump: current_ratio should
        // still be well short of the target (one-pole glide, rate 0.05/chunk).
        assert!(
            src.current_ratio < 1.05,
            "ratio stepped instead of gliding: {}",
            src.current_ratio
        );
    }
}
