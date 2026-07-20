//! Fixed-ratio sample rate conversion (P1). Variable-ratio drift correction
//! (`set_ratio`) is a P2 concern layered on top of a different resampler — see
//! `.lattice/context/drift-and-recovery.md`.

use crate::sample::{DomainError, Format};
use rubato::{FftFixedInOut, Resampler};

/// Progress of one [`Src::process`] call, in samples (interleaved elements,
/// i.e. `frames * channels`) — matches the unit callers already index `input`/`output` in.
pub struct SrcProgress {
    pub consumed: usize,
    pub produced: usize,
}

enum Inner {
    /// `from == to` sample rate — pass-through, no resampler needed.
    Identity,
    Resample(Box<ResampleState>),
}

struct ResampleState {
    resampler: FftFixedInOut<f32>,
    channels: usize,
    chunk_in: usize,
    in_deint: Vec<Vec<f32>>,
    out_deint: Vec<Vec<f32>>,
    /// Interleaved leftover input not yet forming a full `chunk_in` — capacity `chunk_in * channels`.
    pending_in: Vec<f32>,
    pending_in_frames: usize,
    /// Interleaved output produced but not yet delivered to a caller — capacity `chunk_out * channels`.
    /// Only ever holds at most one chunk: the process loop never resamples another chunk
    /// until this one is fully drained (see invariant note in `process`).
    pending_out: Vec<f32>,
    pending_out_frames: usize,
    pending_out_read: usize,
}

pub struct Src {
    inner: Inner,
}

impl Src {
    pub fn new(from: Format, to: Format, max_block_frames: usize) -> Result<Src, DomainError> {
        if from.channels != to.channels {
            return Err(DomainError::ChannelMismatch {
                from: from.channels,
                to: to.channels,
            });
        }
        if from.sample_rate == to.sample_rate {
            return Ok(Src {
                inner: Inner::Identity,
            });
        }

        let channels = from.channels as usize;
        let resampler = FftFixedInOut::<f32>::new(
            from.sample_rate as usize,
            to.sample_rate as usize,
            max_block_frames.max(1),
            channels,
        )
        .map_err(|e| DomainError::ResamplerInit(e.to_string()))?;

        let chunk_in = resampler.input_frames_next();
        let chunk_out = resampler.output_frames_next();

        Ok(Src {
            inner: Inner::Resample(Box::new(ResampleState {
                in_deint: vec![vec![0.0f32; chunk_in]; channels],
                out_deint: vec![vec![0.0f32; chunk_out]; channels],
                pending_in: vec![0.0f32; chunk_in * channels],
                pending_in_frames: 0,
                pending_out: vec![0.0f32; chunk_out * channels],
                pending_out_frames: 0,
                pending_out_read: 0,
                resampler,
                channels,
                chunk_in,
            })),
        })
    }

    /// RT-safe: preallocated at construction, no allocation on this path.
    /// `input`/`output` must each hold a whole number of frames (`len % channels == 0`).
    pub fn process(&mut self, input: &[f32], output: &mut [f32]) -> SrcProgress {
        match &mut self.inner {
            Inner::Identity => {
                let n = input.len().min(output.len());
                output[..n].copy_from_slice(&input[..n]);
                SrcProgress {
                    consumed: n,
                    produced: n,
                }
            }
            Inner::Resample(state) => state.process(input, output),
        }
    }
}

impl ResampleState {
    fn process(&mut self, input: &[f32], output: &mut [f32]) -> SrcProgress {
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

    /// Deinterleaves one full `pending_in` chunk, resamples it, and
    /// interleaves the result into `pending_out`. Caller must ensure
    /// `pending_in_frames == chunk_in` before calling.
    fn resample_pending_chunk(&mut self) {
        for (ch, chan_buf) in self.in_deint.iter_mut().enumerate() {
            for (i, sample) in chan_buf.iter_mut().enumerate() {
                *sample = self.pending_in[i * self.channels + ch];
            }
        }
        self.pending_in_frames = 0;

        let (_, out_len) = self
            .resampler
            .process_into_buffer(&self.in_deint, &mut self.out_deint, None)
            .expect("fixed-chunk resample with correctly sized preallocated buffers");

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

    #[test]
    fn identity_passthrough_matches_input_exactly() {
        let fmt = Format {
            sample_rate: 48_000,
            channels: 2,
        };
        let mut src = Src::new(fmt, fmt, 256).unwrap();
        let input: Vec<f32> = (0..64).map(|i| i as f32 * 0.01).collect();
        let mut output = vec![0.0f32; 64];
        let progress = src.process(&input, &mut output);
        assert_eq!(progress.consumed, 64);
        assert_eq!(progress.produced, 64);
        assert_eq!(input, output);
    }

    #[test]
    fn channel_mismatch_is_rejected() {
        let from = Format {
            sample_rate: 48_000,
            channels: 2,
        };
        let to = Format {
            sample_rate: 48_000,
            channels: 1,
        };
        assert!(matches!(
            Src::new(from, to, 256),
            Err(DomainError::ChannelMismatch { from: 2, to: 1 })
        ));
    }

    #[test]
    fn resample_never_overruns_caller_buffers() {
        let from = Format {
            sample_rate: 44_100,
            channels: 2,
        };
        let to = Format {
            sample_rate: 48_000,
            channels: 2,
        };
        let mut src = Src::new(from, to, 512).unwrap();
        let input = vec![0.0f32; 512 * 2];
        let mut output = vec![0.0f32; 512 * 2 * 2]; // generous headroom
        for _ in 0..8 {
            let progress = src.process(&input, &mut output);
            assert!(progress.consumed <= input.len());
            assert!(progress.produced <= output.len());
        }
    }

    #[test]
    fn silence_in_produces_silence_out() {
        let from = Format {
            sample_rate: 44_100,
            channels: 1,
        };
        let to = Format {
            sample_rate: 48_000,
            channels: 1,
        };
        let mut src = Src::new(from, to, 512).unwrap();
        let input = vec![0.0f32; 512];
        let mut output = vec![1.0f32; 512 * 2]; // pre-fill with non-zero to prove it gets written
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
}
