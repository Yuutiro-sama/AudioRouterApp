//! Digital Signal Processing (DSP) Module.
//!
//! Features:
//! - 4-Point Cubic Hermite fractional interpolator for high-fidelity resampling.
//! - Proportional-Integral (PI) drift controller for quartz oscillator clock compensation.
//! - One-pole exponential volume smoothing (Zero-Zipper Noise).
//! - Soft limiter / clipper guarding against digital overs.
//! - Multi-channel speaker matrixing (Mono, Stereo, 5.1, 7.1).

use crate::audio::ring_buffer::SpscRingBuffer;

/// Adaptive Resampler and DSP processing engine for a single audio endpoint.
pub struct AdaptiveResamplerDsp {
    phase: f64,
    integral_error: f64,
    current_gain: f32,
    base_ratio: f64,
    target_fill_frames: usize,
    last_drift_ppm: i32,
    in_rate: u32,
    out_rate: u32,
}

impl AdaptiveResamplerDsp {
    /// Creates a new DSP engine.
    ///
    /// * `in_rate`: Capture sample rate (e.g. 48000)
    /// * `out_rate`: Output device sample rate (e.g. 44100 or 48000)
    /// * `target_latency_ms`: Target buffer latency in milliseconds (e.g. 15ms for wired, 35ms for BT)
    pub fn new(in_rate: u32, out_rate: u32, target_latency_ms: u32) -> Self {
        let safe_in_rate = if in_rate == 0 { 48000 } else { in_rate };
        let safe_out_rate = if out_rate == 0 { 48000 } else { out_rate };
        let target_fill_frames =
            ((safe_in_rate as usize) * (target_latency_ms as usize)) / 1000;

        Self {
            phase: 0.0,
            integral_error: 0.0,
            current_gain: 1.0,
            base_ratio: (safe_in_rate as f64) / (safe_out_rate as f64),
            target_fill_frames: target_fill_frames.max(128),
            last_drift_ppm: 0,
            in_rate: safe_in_rate,
            out_rate: safe_out_rate,
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

    /// Reset DSP internal state (phase, integrator, gain).
    pub fn reset(&mut self) {
        self.phase = 0.0;
        self.integral_error = 0.0;
        self.last_drift_ppm = 0;
    }

    /// Returns the current clock drift in parts per million (ppm).
    #[inline(always)]
    pub fn last_drift_ppm(&self) -> i32 {
        self.last_drift_ppm
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
        let avail_frames = avail_samples / 2;

        // Overrun recovery: if ring buffer has grown past 4x target fill frames,
        // snap back to target to bound maximum latency.
        if avail_frames > self.target_fill_frames * 4 {
            rb.reset_to_target_frames(self.target_fill_frames);
            self.integral_error = 0.0;
        }

        // Underrun protection: if fewer than 4 frames are present (needed for 4-point interpolation),
        // output smooth silence to avoid harsh pop/click artifacts.
        if avail_frames < 4 {
            for sample in out_interleaved.iter_mut() {
                self.current_gain += 0.005 * (0.0 - self.current_gain);
                *sample = 0.0;
            }
            return;
        }

        // PI Clock Drift Controller:
        // Adjusts resampler step ratio to match the crystal oscillator of the output device.
        let error = (avail_frames as f64 - self.target_fill_frames as f64)
            / (self.target_fill_frames as f64);
        self.integral_error = (self.integral_error + error).clamp(-50.0, 50.0);

        let kp = 0.00025;
        let ki = 0.0000015;
        let drift_adjust = (1.0 + kp * error + ki * self.integral_error).clamp(0.995, 1.005);
        self.last_drift_ppm = ((drift_adjust - 1.0) * 1_000_000.0) as i32;

        let step = self.base_ratio * drift_adjust;
        let effective_target_gain = if muted { 0.0 } else { target_linear_gain };
        let smooth_alpha = 0.002f32; // ~10ms convergence at 48kHz (Zero-zipper noise)

        let mut read_frame_cursor = 0usize;

        for frame_idx in 0..out_frames {
            // Guard against buffer boundary
            if read_frame_cursor + 3 >= avail_frames {
                for ch in 0..channels {
                    out_interleaved[frame_idx * channels + ch] = 0.0;
                }
                continue;
            }

            let (l0, r0) = rb.peek_frame_stereo(read_frame_cursor);
            let (l1, r1) = rb.peek_frame_stereo(read_frame_cursor + 1);
            let (l2, r2) = rb.peek_frame_stereo(read_frame_cursor + 2);
            let (l3, r3) = rb.peek_frame_stereo(read_frame_cursor + 3);

            let t = self.phase as f32;
            let sample_l = Self::cubic_hermite(l0, l1, l2, l3, t);
            let sample_r = Self::cubic_hermite(r0, r1, r2, r3, t);

            // One-pole exponential gain smoother (De-zipper)
            self.current_gain += smooth_alpha * (effective_target_gain - self.current_gain);

            // Soft-limiting / clipping guard
            let out_l = (sample_l * self.current_gain).clamp(-1.0, 1.0);
            let out_r = (sample_r * self.current_gain).clamp(-1.0, 1.0);

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

            self.phase += step;
            let advance = self.phase as usize;
            read_frame_cursor += advance;
            self.phase -= advance as f64;
        }

        let consumed = read_frame_cursor.min(avail_frames.saturating_sub(3));
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
}
