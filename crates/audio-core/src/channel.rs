//! N->M channel-layout conversion (downmix/upmix). ITU-R BS.775-style
//! coefficients: same-speaker pass-through at unity, center/surrounds fold
//! to the nearest available speaker at -3 dB, LFE is dropped. Infallible —
//! an input speaker position the output layout doesn't recognize folds into
//! every output channel at -3 dB rather than being silently dropped or
//! rejected. See `.lattice/context/channel-mixdown.md` for the design.

use crate::sample::ChannelLayout;

/// -3 dB as a linear gain (`1/sqrt(2)`) — the standard BS.775 fold-down level.
const FOLD_3DB: f32 = std::f32::consts::FRAC_1_SQRT_2;

/// Pre-allocated `out_ch x in_ch` coefficient matrix. `ChannelMatrix::new` is
/// the only place this crate does channel-layout math; `process` is a plain
/// multiply-accumulate loop, safe to call on the RT mixer thread.
pub struct ChannelMatrix {
    in_ch: usize,
    out_ch: usize,
    /// Row-major: `coef[m * in_ch + n]` is input channel `n`'s contribution
    /// to output channel `m`. Empty when `identity` is true.
    coef: Vec<f32>,
    identity: bool,
}

impl ChannelMatrix {
    /// Builds the conversion matrix for `from -> to`. Never fails: layouts
    /// this function doesn't have an explicit rule for fold into every
    /// output channel at -3 dB rather than being dropped (capability: a
    /// channel-count/layout mismatch must never hard-fail construction).
    pub fn new(from: ChannelLayout, to: ChannelLayout) -> ChannelMatrix {
        let in_ch = from.count() as usize;
        let out_ch = to.count() as usize;

        if from == to {
            return ChannelMatrix {
                in_ch,
                out_ch,
                coef: Vec::new(),
                identity: true,
            };
        }

        let in_speakers = from.speakers();
        let out_speakers = to.speakers();

        let mut coef = vec![0.0f32; out_ch * in_ch];
        for (col, &spk) in in_speakers.iter().enumerate() {
            for (target, gain) in fold_targets(spk, &out_speakers) {
                if let Some(row) = out_speakers.iter().position(|&s| s == target) {
                    coef[row * in_ch + col] += gain;
                }
            }
        }

        normalize(&mut coef, in_ch);

        ChannelMatrix {
            in_ch,
            out_ch,
            coef,
            identity: false,
        }
    }

    pub fn is_identity(&self) -> bool {
        self.identity
    }

    /// `input` holds whole frames at `in_ch`; `output` must have room for the
    /// same frame count at `out_ch`. Returns samples (not frames) written.
    /// Always overwrites `output` — never accumulates into it, since a stale
    /// scratch buffer would otherwise leak into the result.
    pub fn process(&self, input: &[f32], output: &mut [f32]) -> usize {
        if self.identity {
            let n = input.len().min(output.len());
            output[..n].copy_from_slice(&input[..n]);
            return n;
        }

        debug_assert_eq!(input.len() % self.in_ch.max(1), 0);
        let in_frames = input.len() / self.in_ch.max(1);
        let out_frames = output.len() / self.out_ch.max(1);
        let frames = in_frames.min(out_frames);

        for f in 0..frames {
            for m in 0..self.out_ch {
                let mut acc = 0.0f32;
                for n in 0..self.in_ch {
                    acc += self.coef[m * self.in_ch + n] * input[f * self.in_ch + n];
                }
                output[f * self.out_ch + m] = acc;
            }
        }
        frames * self.out_ch
    }
}

/// Where one input speaker's signal goes and at what gain, given the set of
/// speaker bits present in the output layout. Ordered by precedence: exact
/// match first, then the nearest BS.775-style fold target, then (falling
/// through every guard) the unknown-position catch-all.
fn fold_targets(spk: u32, out: &[u32]) -> Vec<(u32, f32)> {
    use crate::sample::speaker::*;
    let has = |s: u32| out.contains(&s);

    match spk {
        FL if has(FL) => vec![(FL, 1.0)],
        FL if has(FC) => vec![(FC, 1.0)], // no front-left in output: fold fully into center
        FR if has(FR) => vec![(FR, 1.0)],
        FR if has(FC) => vec![(FC, 1.0)],
        FC if has(FC) => vec![(FC, 1.0)],
        // center -> stereo/wider. Guarded (not `FC => ...`): if the output has
        // neither FL nor FR either, this must fall through to the unknown-
        // position catch-all below rather than silently dropping the center.
        FC if has(FL) || has(FR) => vec![(FL, FOLD_3DB), (FR, FOLD_3DB)],
        LFE if has(LFE) => vec![(LFE, 1.0)],
        LFE => vec![], // dropped — never mixed into mains (approved decision)
        BL | SL if has(spk) => vec![(spk, 1.0)],
        BL | SL if has(FL) => vec![(FL, FOLD_3DB)],
        BR | SR if has(spk) => vec![(spk, 1.0)],
        BR | SR if has(FR) => vec![(FR, FOLD_3DB)],
        BC if has(BC) => vec![(BC, 1.0)],
        BC if has(BL) && has(BR) => vec![(BL, FOLD_3DB), (BR, FOLD_3DB)],
        BC if has(SL) && has(SR) => vec![(SL, FOLD_3DB), (SR, FOLD_3DB)],
        BC if has(FL) && has(FR) => vec![(FL, FOLD_3DB), (FR, FOLD_3DB)],
        FLC if has(FLC) => vec![(FLC, 1.0)],
        FLC if has(FL) => vec![(FL, 1.0)],
        FRC if has(FRC) => vec![(FRC, 1.0)],
        FRC if has(FR) => vec![(FR, 1.0)],
        _ => out.iter().map(|&s| (s, FOLD_3DB)).collect(), // unknown position: never lost
    }
}

/// Scales the whole matrix by one global factor (not per row) so a row
/// summing above unity can't clip — per-row normalization would change
/// inter-channel balance instead of just guarding against clipping.
fn normalize(coef: &mut [f32], in_ch: usize) {
    if in_ch == 0 {
        return;
    }
    let out_ch = coef.len() / in_ch;
    let max_row_sum = (0..out_ch)
        .map(|m| coef[m * in_ch..(m + 1) * in_ch].iter().sum::<f32>())
        .fold(0.0f32, f32::max);
    if max_row_sum > 1.0 {
        for c in coef.iter_mut() {
            *c /= max_row_sum;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_layouts_are_identity_and_skip_the_matrix() {
        let m = ChannelMatrix::new(ChannelLayout::STEREO, ChannelLayout::STEREO);
        assert!(m.is_identity());
        let input = [0.1f32, 0.2, 0.3, 0.4];
        let mut output = [0.0f32; 4];
        assert_eq!(m.process(&input, &mut output), 4);
        assert_eq!(output, input);
    }

    #[test]
    fn two_different_six_channel_layouts_are_not_treated_as_identity() {
        // 5.1 vs a hypothetical 6ch layout with a different speaker set —
        // same channel COUNT must not short-circuit to identity.
        let six_ch_other = ChannelLayout::from_mask(0x0000_007E, 6); // FR..BC, no FL
        assert_ne!(ChannelLayout::SURROUND_5_1, six_ch_other);
        let m = ChannelMatrix::new(ChannelLayout::SURROUND_5_1, six_ch_other);
        assert!(!m.is_identity());
    }

    #[test]
    fn mono_to_stereo_center_channel_folds_at_minus_3db_into_both_sides() {
        let m = ChannelMatrix::new(ChannelLayout::MONO, ChannelLayout::STEREO);
        let input = [1.0f32]; // one frame, FC only
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        assert!((output[0] - FOLD_3DB).abs() < 1e-6, "L = {}", output[0]);
        assert!((output[1] - FOLD_3DB).abs() < 1e-6, "R = {}", output[1]);
    }

    #[test]
    fn stereo_to_mono_is_half_sum_after_normalization() {
        let m = ChannelMatrix::new(ChannelLayout::STEREO, ChannelLayout::MONO);
        let input = [1.0f32, 1.0]; // one frame, L=1, R=1
        let mut output = [0.0f32; 1];
        m.process(&input, &mut output);
        assert!((output[0] - 1.0).abs() < 1e-6, "expected normalized 0.5+0.5=1.0, got {}", output[0]);
    }

    #[test]
    fn five_one_to_stereo_center_only_input_lands_equal_on_both_outputs() {
        let m = ChannelMatrix::new(ChannelLayout::SURROUND_5_1, ChannelLayout::STEREO);
        // order: FL FR FC LFE BL BR — only FC hot.
        let input = [0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0];
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        assert!((output[0] - output[1]).abs() < 1e-6, "L={} R={}", output[0], output[1]);
        assert!(output[0] > 0.0, "center must reach the stereo output");
    }

    #[test]
    fn five_one_to_stereo_left_surround_only_input_never_leaks_to_the_right() {
        let m = ChannelMatrix::new(ChannelLayout::SURROUND_5_1, ChannelLayout::STEREO);
        // order: FL FR FC LFE BL BR — only BL (left surround) hot.
        let input = [0.0f32, 0.0, 0.0, 0.0, 1.0, 0.0];
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        assert!(output[0] > 0.0, "left surround must reach L");
        assert_eq!(output[1], 0.0, "left surround must never leak into R");
    }

    #[test]
    fn five_one_to_stereo_lfe_is_dropped_entirely() {
        let m = ChannelMatrix::new(ChannelLayout::SURROUND_5_1, ChannelLayout::STEREO);
        // order: FL FR FC LFE BL BR — only LFE hot.
        let input = [0.0f32, 0.0, 0.0, 1.0, 0.0, 0.0];
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        assert_eq!(output, [0.0, 0.0], "LFE must never reach stereo mains");
    }

    #[test]
    fn matrix_never_clips_a_full_scale_5_1_signal_downmixed_to_stereo() {
        let m = ChannelMatrix::new(ChannelLayout::SURROUND_5_1, ChannelLayout::STEREO);
        let input = [1.0f32; 6]; // every channel at full scale simultaneously
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        for &s in &output {
            assert!(s <= 1.0 + 1e-6, "row not normalized against clipping: {s}");
        }
    }

    #[test]
    fn quad_and_five_one_place_back_left_at_different_columns() {
        // Regression for the column-index gotcha: BL (0x10) is column 4 in
        // 5.1 (mask 0x3F, speakers FL,FR,FC,LFE,BL,BR) but column 2 in quad
        // (mask 0x33, speakers FL,FR,BL,BR). A matrix that assumed "column =
        // bit position" would silently misroute this.
        let quad_speakers = ChannelLayout::QUAD.speakers();
        let five_one_speakers = ChannelLayout::SURROUND_5_1.speakers();
        assert_eq!(quad_speakers[2], 0x10, "BL should be column 2 in quad");
        assert_eq!(five_one_speakers[4], 0x10, "BL should be column 4 in 5.1");
    }

    #[test]
    fn upmix_stereo_to_quad_passes_front_through_and_leaves_rear_silent() {
        let m = ChannelMatrix::new(ChannelLayout::STEREO, ChannelLayout::QUAD);
        let input = [0.5f32, 0.7]; // one frame: L=0.5, R=0.7
        let mut output = [9.0f32; 4]; // pre-fill non-zero to prove overwrite
        m.process(&input, &mut output);
        // QUAD speaker order: FL, FR, BL, BR
        assert!((output[0] - 0.5).abs() < 1e-6, "FL");
        assert!((output[1] - 0.7).abs() < 1e-6, "FR");
        assert_eq!(output[2], 0.0, "BL must be silent, not synthesized");
        assert_eq!(output[3], 0.0, "BR must be silent, not synthesized");
    }

    #[test]
    fn unknown_position_input_channel_folds_into_every_output_not_dropped() {
        // An input mask bit this crate has no named rule for (e.g. a height
        // channel) must still reach every output channel — construction and
        // processing must never silently lose audio.
        let weird_in = ChannelLayout::from_mask(0x1 | 0x2 | 0x1000, 3); // FL, FR, + one unknown bit
        let m = ChannelMatrix::new(weird_in, ChannelLayout::STEREO);
        let input = [0.0f32, 0.0, 1.0]; // only the unknown channel hot
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        assert!(output[0] > 0.0 && output[1] > 0.0, "unknown channel must reach both outputs");
    }

    #[test]
    fn center_channel_reaches_output_even_when_layout_has_no_front_speakers_at_all() {
        // Regression: FL/FR/FC each had an unguarded fallback arm that
        // matched unconditionally once the exact-match guard failed, so if
        // the fallback's own target (FC, or FL+FR) was ALSO absent from the
        // output layout, the match returned an empty contribution instead of
        // falling through to the unknown-position catch-all. BL/SL/BR/SR
        // never had this bug — only FL/FR/FC did.
        let side_only = ChannelLayout::from_mask(0x200 | 0x400, 2); // SL|SR, no FC/FL/FR
        let m = ChannelMatrix::new(ChannelLayout::MONO, side_only);
        let input = [1.0f32]; // FC only
        let mut output = [0.0f32; 2];
        m.process(&input, &mut output);
        assert!(
            output[0] > 0.0 && output[1] > 0.0,
            "center must still reach every output channel via the catch-all, got {output:?}"
        );
    }

    #[test]
    fn process_never_accumulates_into_a_dirty_output_buffer() {
        let m = ChannelMatrix::new(ChannelLayout::SURROUND_5_1, ChannelLayout::STEREO);
        let input = [0.0f32; 6]; // silence in
        let mut output = [1.0f32; 2]; // dirty scratch, not zeroed by caller
        m.process(&input, &mut output);
        assert_eq!(output, [0.0, 0.0], "process must overwrite, never += into stale scratch");
    }
}
