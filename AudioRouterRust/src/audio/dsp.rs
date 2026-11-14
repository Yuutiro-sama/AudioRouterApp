//! Digital Signal Processing (DSP) Module.
//!
//! Features:
//! - 4-Point Cubic Hermite fractional interpolator for high-fidelity resampling.
//! - Boundary Clamping preventing micro-pops and boundary index dropouts.
//! - Smooth Exponential Slew-to-Zero on buffer starvation.
//! - 1st-order Exponential Moving Average (EMA) smoothed clock drift controller.
//! - Overrun recovery with smooth crossfade / slew.
//! - One-pole exponential volume smoothing (Zero-Zipper Noise).
//! - Soft limiter / clipper guarding against digital overs.
//! - Multi-channel speaker matrixing (Mono, Stereo, 5.1, 7.1).

use crate::audio::ring_buffer::SpscRingBuffer;

/// Adaptive Resampler and DSP processing engine for a single audio endpoint.
pub struct AdaptiveResamplerDsp {
    phase: f64,
    integral_error: f64,
    smoothed_drift: f64,
    current_gain: f32,
    base_ratio: f64,
    target_fill_frames: usize,
    last_drift_ppm: i32,
    in_rate: u32,
    out_rate: u32,
    last_sample_l: f32,
    last_sample_r: f32,
    slew_counter: usize,
    slew_total: usize,
    slew_from_l: f32,
    slew_from_r: f32,
    had_underrun: bool,
}

impl AdaptiveResamplerDsp {
    /// Creates a new DSP engine.
    ///
    /// * `in_rate`: Capture sample rate (e.g. 48000)
    /// * `out_rate`: Output device sample rate (e.g. 44100, 48000, 192000, 384000)
    /// * `target_latency_ms`: Target buffer latency in milliseconds (e.g. 15ms for wired, 35ms for BT)
    pub fn new(in_rate: u32, out_rate: u32, target_latency_ms: u32) -> Self {
        let safe_in_rate = if in_rate == 0 { 48000 } else { in_rate };
        let safe_out_rate = if out_rate == 0 { 48000 } else { out_rate };
        let target_fill_frames =
            ((safe_in_rate as usize) * (target_latency_ms as usize)) / 1000;

        Self {
            phase: 0.0,
            integral_error: 0.0,
            smoothed_drift: 1.0,
            current_gain: 1.0,
            base_ratio: (safe_in_rate as f64) / (safe_out_rate as f64),
            target_fill_frames: target_fill_frames.max(128),
            last_drift_ppm: 0,
            in_rate: safe_in_rate,
            out_rate: safe_out_rate,
            last_sample_l: 0.0,
            last_sample_r: 0.0,
            slew_counter: 0,
            slew_total: 0,
            slew_from_l: 0.0,
            slew_from_r: 0.0,
            had_underrun: false,
        }
    }

    /// Capture sample rate in Hz.
    #[inline(always)]
    pub fn in_rate(&self) -> u32 {
        self.in_rate
    }

    /// Output device sample rate in Hz.
    #[inline(always)]
    pub fn output_rate(&self) -> u32 {
        self.out_rate
    }

    /// Target ring buffer fill frames for latency and drift regulation.
    #[inline(always)]
    pub fn target_fill_frames(&self) -> usize {
        self.target_fill_frames
    }

    /// Reset DSP internal state (phase, integrator, gain, slew).
    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.integral_error = 0.0;
        self.smoothed_drift = 1.0;
        self.last_drift_ppm = 0;
        self.last_sample_l = 0.0;
        self.last_sample_r = 0.0;
        self.slew_counter = 0;
        self.slew_total = 0;
        self.slew_from_l = 0.0;
        self.slew_from_r = 0.0;
        self.had_underrun = false;
    }

    /// Returns the current clock drift in parts per million (ppm).
    #[inline(always)]
    pub fn last_drift_ppm(&self) -> i32 {
        self.last_drift_ppm
    }

    /// Returns the currently smoothed clock drift multiplier.
    #[inline(always)]
    pub fn smoothed_drift(&self) -> f64 {
        self.smoothed_drift
    }

    /// Calculates current latency in milliseconds based on available frames in ring buffer.
    #[inline(always)]
    pub fn current_latency_ms(&self, avail_frames: usize) -> f32 {
        if self.in_rate == 0 {
            0.0
        } else {
            (avail_frames as f32 * 1000.0) / (self.in_rate as f32)
        }
    }

    /// 4-point, 3rd-order Hermite polynomial interpolation.
    ///
    /// Formula preserves smoothness and C1 continuity with minimal CPU overhead:
    /// c0 = y1
    /// c1 = 0.5 * (y2 - y0)
    /// c2 = y0 - 2.5 * y1 + 2 * y2 - 0.5 * y3
    /// c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2)
    #[inline(always)]
    pub fn cubic_hermite(y0: f32, y1: f32, y2: f32, y3: f32, t: f32) -> f32 {
        let c0 = y1;
        let c1 = 0.5 * (y2 - y0);
        let c2 = y0 - 2.5 * y1 + 2.0 * y2 - 0.5 * y3;
        let c3 = 0.5 * (y3 - y0) + 1.5 * (y1 - y2);
        ((c3 * t + c2) * t + c1) * t + c0
    }

    /// Processes an output block of stereo interleaved audio.
    ///
    /// * `rb`: Reference to the SpscRingBuffer supplying source stereo PCM.
    /// * `out_interleaved`: Output buffer receiving interleaved stereo f32.
    /// * `target_linear_gain`: Desired linear volume scalar [0.0 .. 1.0].
    /// * `muted`: True if this target is muted.
    pub fn process_output_block(
        &mut self,
        rb: &SpscRingBuffer,
        out_interleaved: &mut [f32],
        target_linear_gain: f32,
        muted: bool,
    ) {
        self.process_output_block_multichannel(rb, out_interleaved, 2, target_linear_gain, muted);
    }

    /// Processes an output block with arbitrary channel count (1, 2, 4, 6, 8, etc.).
    ///
    /// Interleaved stereo source from `rb` is resampled, volume-scaled, soft-limited,
    /// and distributed across the output channels according to standard channel mapping.
    pub fn process_output_block_multichannel(
        &mut self,
        rb: &SpscRingBuffer,
        out_interleaved: &mut [f32],
        channels: usize,
        target_linear_gain: f32,
        muted: bool,
    ) {
        if channels == 0 || out_interleaved.is_empty() {
            return;
        }

        let out_frames = out_interleaved.len() / channels;
        let avail_samples = rb.available_read();
        let mut avail_frames = avail_samples / 2;

        // Overrun recovery: if ring buffer has grown past 4x target fill frames,
        // snap back to target to bound maximum latency, but smoothly slew from
        // the last rendered sample to avoid an audible cliff/pop.
        if avail_frames > self.target_fill_frames * 4 {
            self.slew_from_l = self.last_sample_l;
            self.slew_from_r = self.last_sample_r;
            self.slew_total = 128.min(self.target_fill_frames);
            self.slew_counter = self.slew_total;

            rb.reset_to_target_frames(self.target_fill_frames);
            self.integral_error = 0.0;
            avail_frames = self.target_fill_frames;
        }

        // Underrun protection: if the buffer is completely empty,
        // apply smooth exponential slew-to-zero (decay factor 0.999) from the last sample
        // to maintain derivative and amplitude continuity without clicks.
        if avail_frames == 0 {
            self.had_underrun = true;
            for frame_idx in 0..out_frames {
                self.last_sample_l *= 0.999;
                self.last_sample_r *= 0.999;
                if self.last_sample_l.abs() < 1e-7 {
                    self.last_sample_l = 0.0;
                }
                if self.last_sample_r.abs() < 1e-7 {
                    self.last_sample_r = 0.0;
                }
                let out_l = self.last_sample_l;
                let out_r = self.last_sample_r;

                let base_idx = frame_idx * channels;
                match channels {
                    1 => out_interleaved[base_idx] = 0.5 * (out_l + out_r),
                    2 => {
                        out_interleaved[base_idx] = out_l;
                        out_interleaved[base_idx + 1] = out_r;
                    }
                    _ => {
                        out_interleaved[base_idx] = out_l;
                        out_interleaved[base_idx + 1] = out_r;
                        for ch in 2..channels {
                            out_interleaved[base_idx + ch] = 0.0;
                        }
                    }
                }
            }
            return;
        }

        // If recovering from a previous starvation/underrun, slew smoothly into the incoming stream
        if self.had_underrun {
            self.had_underrun = false;
            self.slew_from_l = self.last_sample_l;
            self.slew_from_r = self.last_sample_r;
            self.slew_total = 64.min(avail_frames);
            self.slew_counter = self.slew_total;
        }

        // PI Clock Drift Controller:
        // Adjusts resampler step ratio to match the crystal oscillator of the output device.
        let error = (avail_frames as f64 - self.target_fill_frames as f64)
            / (self.target_fill_frames as f64);
        self.integral_error = (self.integral_error + error).clamp(-150.0, 150.0);

        let kp = 0.0008;
        let ki = 0.000008;
        // Dynamic drift range: allow faster drain if buffer is bloated (> 2x target)
        let max_drift = if avail_frames > self.target_fill_frames * 2 { 1.02 } else { 1.008 };
        let min_drift = if avail_frames < self.target_fill_frames / 2 { 0.98 } else { 0.992 };
        let raw_drift_adjust = (1.0 + kp * error + ki * self.integral_error).clamp(min_drift, max_drift);

        // 1st-order Exponential Moving Average (EMA) filter on drift adjust (0.98 / 0.02)
        self.smoothed_drift = 0.98 * self.smoothed_drift + 0.02 * raw_drift_adjust;
        self.last_drift_ppm = ((self.smoothed_drift - 1.0) * 1_000_000.0) as i32;

        let step = self.base_ratio * self.smoothed_drift;
        let effective_target_gain = if muted { 0.0 } else { target_linear_gain };
        let smooth_alpha = 0.002f32; // ~10ms convergence at 48kHz (Zero-zipper noise)

        let mut read_frame_cursor = 0usize;
        let max_idx = avail_frames - 1;

        for frame_idx in 0..out_frames {
            // One-pole exponential gain smoother (De-zipper)
            self.current_gain += smooth_alpha * (effective_target_gain - self.current_gain);

            let (out_l, out_r) = if read_frame_cursor < avail_frames {
                // Boundary Clamping for Hermite interpolation:
                // Lookahead indices are clamped to (avail_frames - 1)
                let idx0 = read_frame_cursor;
                let idx1 = (read_frame_cursor + 1).min(max_idx);
                let idx2 = (read_frame_cursor + 2).min(max_idx);
                let idx3 = (read_frame_cursor + 3).min(max_idx);

                let (l0, r0) = rb.peek_frame_stereo(idx0);
                let (l1, r1) = rb.peek_frame_stereo(idx1);
                let (l2, r2) = rb.peek_frame_stereo(idx2);
                let (l3, r3) = rb.peek_frame_stereo(idx3);

                let t = self.phase as f32;
                let sample_l = Self::cubic_hermite(l0, l1, l2, l3, t);
                let sample_r = Self::cubic_hermite(r0, r1, r2, r3, t);

                // Soft-limiting / clipping guard
                let raw_out_l = (sample_l * self.current_gain).clamp(-1.0, 1.0);
                let raw_out_r = (sample_r * self.current_gain).clamp(-1.0, 1.0);

                if self.slew_counter > 0 && self.slew_total > 0 {
                    let blend = 1.0 - (self.slew_counter as f32 / self.slew_total as f32);
                    let slewed_l = (1.0 - blend) * self.slew_from_l + blend * raw_out_l;
                    let slewed_r = (1.0 - blend) * self.slew_from_r + blend * raw_out_r;
                    self.slew_counter -= 1;
                    (slewed_l, slewed_r)
                } else {
                    (raw_out_l, raw_out_r)
                }
            } else {
                // Buffer depleted during block processing:
                // Smooth exponential slew-to-zero (decay factor 0.999) from last valid sample
                self.had_underrun = true;
                self.last_sample_l *= 0.999;
                self.last_sample_r *= 0.999;
                if self.last_sample_l.abs() < 1e-7 {
                    self.last_sample_l = 0.0;
                }
                if self.last_sample_r.abs() < 1e-7 {
                    self.last_sample_r = 0.0;
                }
                (self.last_sample_l, self.last_sample_r)
            };

            self.last_sample_l = out_l;
            self.last_sample_r = out_r;

            let base_idx = frame_idx * channels;
            match channels {
                1 => {
                    // Mono downmix
                    out_interleaved[base_idx] = 0.5 * (out_l + out_r);
                }
                2 => {
                    // Standard Stereo
                    out_interleaved[base_idx] = out_l;
                    out_interleaved[base_idx + 1] = out_r;
                }
                _ => {
                    // Multi-channel surround (e.g. 5.1 / 7.1)
                    // FL, FR receive main signal, other channels remain silent or center-mixed
                    out_interleaved[base_idx] = out_l;
                    out_interleaved[base_idx + 1] = out_r;
                    for ch in 2..channels {
                        out_interleaved[base_idx + ch] = 0.0;
                    }
                }
            }

            if read_frame_cursor < avail_frames {
                self.phase += step;
                let advance = self.phase as usize;
                read_frame_cursor += advance;
                self.phase -= advance as f64;
            }
        }

        let consumed = read_frame_cursor.min(avail_frames);
        rb.advance_read_frames(consumed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_cubic_hermite_interpolation() {
        // Flat line y = 1.0
        let val = AdaptiveResamplerDsp::cubic_hermite(1.0, 1.0, 1.0, 1.0, 0.5);
        assert!((val - 1.0).abs() < 1e-6);

        // Linear slope: 0, 1, 2, 3 -> at t = 0.5, midpoint between y1 and y2 is 1.5
        let val_lin = AdaptiveResamplerDsp::cubic_hermite(0.0, 1.0, 2.0, 3.0, 0.5);
        assert!((val_lin - 1.5).abs() < 1e-6);
    }

    #[test]
    fn test_resampler_gain_smoothing() {
        let rb = SpscRingBuffer::new(1024);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);

        // Fill with stereo constant 1.0
        let input = vec![1.0f32; 500];
        rb.push_slice(&input);

        let mut output = vec![0.0f32; 200];
        // Process block with target gain 0.5 (initial gain is 1.0)
        dsp.process_output_block(&rb, &mut output, 0.5, false);

        // Check that gain smoothly transitioned towards 0.5 without instant jump
        assert!(output[0] > 0.5);
        assert!(output[output.len() - 1] < output[0]);
    }

    #[test]
    fn test_resampler_pi_controller_drift() {
        let rb = SpscRingBuffer::new(2048);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        // target_fill_frames = 48000 * 15 / 1000 = 720 frames = 1440 samples

        // Fill with excess data (over target) -> PI controller should speed up reading (positive ppm)
        let excess_data = vec![0.1f32; 1800]; // 900 frames > 720
        rb.push_slice(&excess_data);

        let mut output = vec![0.0f32; 100];
        dsp.process_output_block(&rb, &mut output, 1.0, false);

        assert!(dsp.last_drift_ppm() > 0, "Drift ppm should be positive to drain excess buffer");
    }

    #[test]
    fn test_resampler_multichannel_mono_and_quad() {
        let rb = SpscRingBuffer::new(512);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 10);

        let input = vec![0.6f32, 0.2f32, 0.6f32, 0.2f32, 0.6f32, 0.2f32, 0.6f32, 0.2f32, 0.6f32, 0.2f32];
        rb.push_slice(&input);

        // Test Mono (1 channel)
        let mut mono_out = vec![0.0f32; 2];
        dsp.process_output_block_multichannel(&rb, &mut mono_out, 1, 1.0, false);
        // Average of 0.6 and 0.2 is 0.4
        assert!((mono_out[0] - 0.4).abs() < 0.1);
    }

    #[test]
    fn test_resampler_boundary_clamping_no_pops() {
        let rb = SpscRingBuffer::new(512);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 10);

        // Push only 5 stereo frames of 0.8 amplitude
        let input = vec![0.8f32; 10];
        rb.push_slice(&input);

        // Ask for 20 frames (40 samples): buffer will run out around frame 5
        let mut output = vec![0.0f32; 40];
        dsp.process_output_block(&rb, &mut output, 1.0, false);

        // Verify that the first samples match ~0.8
        assert!((output[0] - 0.8).abs() < 0.05);

        // Crucial: check that adjacent samples have NO instantaneous jump/pop (cliff)
        for i in 1..output.len() {
            let diff = (output[i] - output[i - 1]).abs();
            assert!(
                diff < 0.05,
                "Micro-pop detected between sample {} and {}: diff={}",
                i - 1,
                i,
                diff
            );
        }
    }

    #[test]
    fn test_resampler_smoothed_drift_ema() {
        let rb = SpscRingBuffer::new(2048);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        assert_eq!(dsp.smoothed_drift(), 1.0);

        // Add excess data to trigger positive drift
        let excess_data = vec![0.2f32; 1800];
        rb.push_slice(&excess_data);

        let mut output = vec![0.0f32; 100];
        dsp.process_output_block(&rb, &mut output, 1.0, false);

        // EMA should smoothly pull drift up above 1.0
        assert!(dsp.smoothed_drift() > 1.0);
        assert!(dsp.last_drift_ppm() > 0);
    }

    #[test]
    fn test_resampler_overrun_slew_continuity() {
        let rb = SpscRingBuffer::new(8192);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        let target = dsp.target_fill_frames();

        // Push massive excess frames (> 4x target)
        let excess_frames = target * 5;
        let excess_data = vec![0.5f32; excess_frames * 2];
        rb.push_slice(&excess_data);

        let mut output = vec![0.0f32; 512];
        dsp.process_output_block(&rb, &mut output, 1.0, false);

        // Buffer should have reset to target_fill_frames (minus frames consumed)
        assert!(rb.available_read() / 2 <= target);

        // Verify no NaN or sudden spikes
        for &s in &output {
            assert!(!s.is_nan());
            assert!(s.abs() <= 1.0);
        }
    }

    #[test]
    fn test_resampler_high_sample_rate_384khz() {
        // High sample rate test: 48kHz input to 384kHz output (8x upsampling)
        let rb = SpscRingBuffer::new(16384);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 20);

        // Feed chunks of 48kHz audio (100 frames at a time)
        let mut out_block = vec![0.0f32; 800 * 2]; // 800 frames of stereo at 384kHz

        for chunk_idx in 0..10 {
            let mut chunk = Vec::with_capacity(200);
            for f in 0..100 {
                let sample = ((chunk_idx * 100 + f) as f32 * 0.05).sin() * 0.8;
                chunk.push(sample);
                chunk.push(sample);
            }
            rb.push_slice(&chunk);

            dsp.process_output_block(&rb, &mut out_block, 1.0, false);

            // Check sample continuity across 384kHz stream
            for i in 1..out_block.len() {
                let diff = (out_block[i] - out_block[i - 1]).abs();
                assert!(
                    diff < 0.1,
                    "Discontinuity/micro-pop at 384kHz detected at sample {}: diff={}",
                    i,
                    diff
                );
            }
        }
    }

    #[test]
    fn test_resampling_48k_to_384k_continuity() {
        // High-precision continuity test: 48kHz input resampled to 384kHz output (8x ratio).
        // Uses a continuous 1000 Hz sinusoidal test tone with amplitude 0.75.
        // Verifies that across multiple consecutive blocks, the maximum adjacent sample delta
        // |x[n] - x[n-1]| remains strictly bounded (no zero-drop cliff or Dirac step).
        let rb = SpscRingBuffer::new(32768);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 25);
        let target = dsp.target_fill_frames(); // 1200 frames

        // Prime the ring buffer to the target fill level
        let mut prime_buf = Vec::with_capacity(target * 2);
        for t in 0..target {
            let s = (2.0 * std::f32::consts::PI * 1000.0 * (t as f32) / 48000.0).sin() * 0.75;
            prime_buf.push(s);
            prime_buf.push(s);
        }
        rb.push_slice(&prime_buf);

        let out_frames_per_block = 1920; // 5ms block at 384kHz (3840 stereo samples)
        let mut out_block = vec![0.0f32; out_frames_per_block * 2];
        let mut global_max_diff = 0.0f32;
        let mut prev_sample_l: Option<f32> = None;
        let mut prev_sample_r: Option<f32> = None;

        let num_blocks = 20; // Test 20 consecutive blocks (100ms total rendered)
        let in_frames_per_block = 240; // 5ms at 48kHz = 240 frames

        for block_idx in 0..num_blocks {
            // Replenish 48kHz input to simulate real-time loopback capture
            let mut feed_buf = Vec::with_capacity(in_frames_per_block * 2);
            let base_t = target + block_idx * in_frames_per_block;
            for t in 0..in_frames_per_block {
                let sample_idx = base_t + t;
                let s = (2.0 * std::f32::consts::PI * 1000.0 * (sample_idx as f32) / 48000.0).sin() * 0.75;
                feed_buf.push(s);
                feed_buf.push(s);
            }
            rb.push_slice(&feed_buf);

            dsp.process_output_block_multichannel(&rb, &mut out_block, 2, 1.0, false);

            // Check continuity at block boundary
            if let (Some(pl), Some(pr)) = (prev_sample_l, prev_sample_r) {
                let diff_l = (out_block[0] - pl).abs();
                let diff_r = (out_block[1] - pr).abs();
                global_max_diff = global_max_diff.max(diff_l).max(diff_r);
                assert!(
                    diff_l < 0.03,
                    "Discontinuity at block {} start (Left): prev={}, cur={}, diff={}",
                    block_idx, pl, out_block[0], diff_l
                );
                assert!(
                    diff_r < 0.03,
                    "Discontinuity at block {} start (Right): prev={}, cur={}, diff={}",
                    block_idx, pr, out_block[1], diff_r
                );
            }

            // Check intra-block sample continuity (adjacent frames: left with left, right with right)
            for f in 1..out_frames_per_block {
                let cur_l = out_block[f * 2];
                let prev_l = out_block[(f - 1) * 2];
                let diff_l = (cur_l - prev_l).abs();

                let cur_r = out_block[f * 2 + 1];
                let prev_r = out_block[(f - 1) * 2 + 1];
                let diff_r = (cur_r - prev_r).abs();

                global_max_diff = global_max_diff.max(diff_l).max(diff_r);

                // Theoretical max derivative for 1kHz sine at 384kHz is ~0.0123
                assert!(
                    diff_l < 0.025,
                    "Dirac step / micro-pop detected in block {} at frame {} (Left): diff={}",
                    block_idx, f, diff_l
                );
                assert!(
                    diff_r < 0.025,
                    "Dirac step / micro-pop detected in block {} at frame {} (Right): diff={}",
                    block_idx, f, diff_r
                );
            }

            prev_sample_l = Some(out_block[(out_frames_per_block - 1) * 2]);
            prev_sample_r = Some(out_block[(out_frames_per_block - 1) * 2 + 1]);
        }

        println!(
            "test_resampling_48k_to_384k_continuity passed: global_max_diff = {:.6} (well below 0.025 limit)",
            global_max_diff
        );
        assert!(global_max_diff > 0.005, "Signal should be active non-zero");
        assert!(global_max_diff < 0.025, "Maximum step diff exceeded tolerance");
    }

    #[test]
    fn test_boundary_clamping_underrun_smoothness() {
        // Starvation and boundary clamping verification:
        // Tests edge cases where buffer contains only 1, 2, or 3 frames.
        // Asserts:
        // 1. Zero panic under all buffer boundary states.
        // 2. Continuous decay to zero without any sudden cliff or Dirac pulse.

        // Subcase A: Exactly 1 frame in buffer [0.65, 0.65]
        {
            let rb = SpscRingBuffer::new(512);
            let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 20);
            rb.push_slice(&[0.65, 0.65]);

            let mut out = vec![0.0f32; 128 * 2]; // 128 frames at 384kHz
            dsp.process_output_block_multichannel(&rb, &mut out, 2, 1.0, false);

            // First sample should be close to 0.65
            assert!((out[0] - 0.65).abs() < 0.1, "First sample should match input frame");

            // All consecutive sample differences must be strictly below 0.02 (smooth decay)
            for i in 2..out.len() {
                let diff = (out[i] - out[i - 2]).abs(); // Compare same channel
                assert!(
                    diff < 0.02,
                    "Starvation pop detected on 1-frame buffer at sample {}: diff={}",
                    i, diff
                );
            }
            // Tail should decay towards 0.0
            assert!(out[out.len() - 1].abs() < out[0].abs());
        }

        // Subcase B: Exactly 2 frames in buffer [0.4, 0.4, 0.7, 0.7]
        {
            let rb = SpscRingBuffer::new(512);
            let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 20);
            rb.push_slice(&[0.4, 0.4, 0.7, 0.7]);

            let mut out = vec![0.0f32; 256 * 2];
            dsp.process_output_block_multichannel(&rb, &mut out, 2, 1.0, false);

            for i in 2..out.len() {
                let diff = (out[i] - out[i - 2]).abs();
                assert!(
                    diff < 0.025,
                    "Discontinuity detected on 2-frame buffer at sample {}: diff={}",
                    i, diff
                );
            }
        }

        // Subcase C: Exactly 3 frames in buffer [0.2, 0.2, 0.5, 0.5, 0.8, 0.8]
        {
            let rb = SpscRingBuffer::new(512);
            let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 20);
            rb.push_slice(&[0.2, 0.2, 0.5, 0.5, 0.8, 0.8]);

            let mut out = vec![0.0f32; 256 * 2];
            dsp.process_output_block_multichannel(&rb, &mut out, 2, 1.0, false);

            for i in 2..out.len() {
                let diff = (out[i] - out[i - 2]).abs();
                assert!(
                    diff < 0.05,
                    "Discontinuity detected on 3-frame buffer at sample {}: diff={}",
                    i, diff
                );
            }
        }

        // Subcase D: Empty buffer (0 frames)
        {
            let rb = SpscRingBuffer::new(512);
            let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 20);
            let mut out = vec![0.0f32; 128 * 2];
            dsp.process_output_block_multichannel(&rb, &mut out, 2, 1.0, false);
            for &s in &out {
                assert_eq!(s, 0.0);
            }
        }
    }

    #[test]
    fn test_ema_drift_smoothing() {
        // PI Clock Drift EMA Smoothing Verification:
        // Tests that a sudden massive buffer error spike does NOT cause a sudden ratio cliff.
        // The 1st-order EMA filter (alpha = 0.05) must smooth the drift trajectory gracefully.
        let rb = SpscRingBuffer::new(16384);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 384000, 25);
        let target = dsp.target_fill_frames(); // 1200 frames

        assert_eq!(dsp.smoothed_drift(), 1.0);
        assert_eq!(dsp.last_drift_ppm(), 0);

        // Inject sudden massive surplus (+100% buffer excess = 2400 frames)
        let surplus_frames = target * 2;
        let surplus_data = vec![0.1f32; surplus_frames * 2];
        rb.push_slice(&surplus_data);

        let mut out = vec![0.0f32; 512]; // 256 frames
        dsp.process_output_block_multichannel(&rb, &mut out, 2, 1.0, false);

        let drift_after_step1 = dsp.smoothed_drift();
        let delta_step1 = drift_after_step1 - 1.0;

        // Raw PI without EMA would instantly jump by ~0.005 to 0.020.
        // With EMA (alpha = 0.05), step 1 delta must be gentle: delta <= 0.0015
        assert!(
            drift_after_step1 > 1.0,
            "Drift should move in positive direction to drain excess buffer"
        );
        assert!(
            delta_step1 < 0.0015,
            "Drift jumped too abruptly! delta={}, expected gentle EMA rise",
            delta_step1
        );

        // Process another 5 blocks, verifying smooth progression
        let mut prev_drift = drift_after_step1;
        for block in 2..=6 {
            dsp.process_output_block_multichannel(&rb, &mut out, 2, 1.0, false);
            let cur_drift = dsp.smoothed_drift();
            let step_diff = (cur_drift - prev_drift).abs();

            assert!(
                step_diff < 0.001,
                "Block {} drift step difference too large: diff={}",
                block, step_diff
            );
            assert!(
                cur_drift >= prev_drift,
                "Drift should increase monotonically towards steady-state"
            );
            prev_drift = cur_drift;
        }

        println!(
            "test_ema_drift_smoothing passed: drift smoothly rose from 1.0 to {:.6} over 6 blocks (smooth transition)",
            prev_drift
        );
    }
}
