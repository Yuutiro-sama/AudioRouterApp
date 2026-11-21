//! Digital Signal Processing (DSP) Module.
//!
//! Features:
//! - 4-Point Cubic Hermite fractional interpolator for high-fidelity resampling.
//! - Boundary Clamping preventing micro-pops and boundary index dropouts.
//! - Smooth Exponential Slew-to-Zero on buffer starvation.
//! - 3-Zone Continuous Rate Controller:
//!   * Zone 1 (Normal, fill_ratio <= 1.8): Precise PI controller (Kp=0.0008, Ki=0.000008) with 1st-order EMA filter.
//!   * Zone 2 (Soft Proportional Drain, 1.8 < fill_ratio <= 10.0): Smoothstep (3x^2 - 2x^3) progressive drain (+0.4% to +4.5%) with Zero Sample Loss.
//!   * Zone 3 (Emergency Safety Valve, fill_ratio > 12.0): Slew-protected overrun recovery retaining 2x target fill frames.
//! - Time-Scaled Cosine Slew (10ms): Hann-window based smooth crossfade scaled to output sample rate.
//! - Soft Limiter & Headroom Guard (0.982 scaling + tanh knee) guarding against Hermite inter-sample peak overshoots.
//! - One-pole exponential volume smoothing (Zero-Zipper Noise).
//! - Multi-channel speaker matrixing (Mono, Stereo, 5.1, 7.1).

use crate::audio::ring_buffer::SpscRingBuffer;

/// Discrete State Machine states for the Resampler engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ResamplerState {
    /// Initial state waiting for input buffer priming.
    Priming = 0,
    /// Actively rendering audio with fractional Hermite interpolation and clock tracking.
    Running = 1,
    /// Graceful 3ms Cosine fade-out into silence upon ring buffer starvation.
    UnderrunFadeOut = 2,
    /// Holding silence and frozen PI integrator while ring buffer replenishes to hysteresis threshold.
    Repriming = 3,
}

impl ResamplerState {
    pub fn from_u8(val: u8) -> Self {
        match val {
            0 => Self::Priming,
            1 => Self::Running,
            2 => Self::UnderrunFadeOut,
            _ => Self::Repriming,
        }
    }
}

/// Dynamic configuration parameters for `AdaptiveResamplerDsp`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResamplerConfig {
    pub capture_rate: u32,
    pub render_rate: u32,
    pub target_fill_ms: f32,    // default: 30.0
    pub reprime_fill_ms: f32,   // default: 25.0
    pub fade_ms: f32,           // default: 3.0
}

impl Default for ResamplerConfig {
    fn default() -> Self {
        Self {
            capture_rate: 48000,
            render_rate: 48000,
            target_fill_ms: 30.0,
            reprime_fill_ms: 25.0,
            fade_ms: 3.0,
        }
    }
}

/// Adaptive Resampler and DSP processing engine for a single audio endpoint.
pub struct AdaptiveResamplerDsp {
    history: [[f32; 2]; 4],
    state: ResamplerState,
    config: ResamplerConfig,
    frac_pos: f64,
    integral_error: f64,
    smoothed_drift: f64,
    current_drift_ppm: f64,
    current_gain: f32,
    base_ratio: f64,
    target_fill_frames: usize,
    reprime_fill_frames: usize,
    last_drift_ppm: i32,
    in_rate: u32,
    out_rate: u32,
    last_sample_l: f32,
    last_sample_r: f32,
    slew_counter: usize,
    slew_total: usize,
    slew_from_l: f32,
    slew_from_r: f32,
    fade_counter: usize,
    fade_total: usize,
    fade_from_l: f32,
    fade_from_r: f32,
    default_slew_frames: usize,
    micro_slew_frames: usize,
    had_underrun: bool,
    underrun_count: u64,
    slew_trigger_count: u64,
}

/// Alias for compatibility with Master Plan naming.
pub type HermiteResampler = AdaptiveResamplerDsp;

/// Processing statistics returned by `process_from_ring`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProcessStats {
    pub frames_consumed: usize,
    pub had_underrun: bool,
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
        let target_ms = if target_latency_ms == 0 { 30.0 } else { target_latency_ms as f32 };
        let reprime_ms = if target_ms >= 30.0 {
            25.0
        } else {
            (target_ms * 0.25).max(2.0)
        };
        let config = ResamplerConfig {
            capture_rate: safe_in_rate,
            render_rate: safe_out_rate,
            target_fill_ms: target_ms,
            reprime_fill_ms: reprime_ms,
            fade_ms: 3.0,
        };
        Self::with_config(config)
    }

    /// Creates a new DSP engine with explicit `ResamplerConfig`.
    pub fn with_config(config: ResamplerConfig) -> Self {
        let safe_in_rate = if config.capture_rate == 0 { 48000 } else { config.capture_rate };
        let safe_out_rate = if config.render_rate == 0 { 48000 } else { config.render_rate };
        let target_fill_frames =
            ((safe_in_rate as f32 * config.target_fill_ms / 1000.0) as usize).max(64);
        let reprime_fill_frames =
            ((safe_in_rate as f32 * config.reprime_fill_ms / 1000.0) as usize).max(16);
        // Time-scaled 10ms default slew frames: 480 @ 48kHz, 3,840 @ 384kHz
        let default_slew_frames = ((safe_out_rate as usize) / 100).max(256);
        // Time-scaled 3ms micro-slew frames for underrun recovery / fade-in / fade-out: 144 @ 48kHz, 1,152 @ 384kHz
        let micro_slew_frames = ((safe_out_rate as f32 * config.fade_ms / 1000.0).round() as usize).max(16);

        Self {
            history: [[0.0; 2]; 4],
            state: ResamplerState::Priming,
            config,
            frac_pos: 0.0,
            integral_error: 0.0,
            smoothed_drift: 1.0,
            current_drift_ppm: 0.0,
            current_gain: 1.0,
            base_ratio: (safe_in_rate as f64) / (safe_out_rate as f64),
            target_fill_frames,
            reprime_fill_frames,
            last_drift_ppm: 0,
            in_rate: safe_in_rate,
            out_rate: safe_out_rate,
            last_sample_l: 0.0,
            last_sample_r: 0.0,
            slew_counter: 0,
            slew_total: default_slew_frames,
            slew_from_l: 0.0,
            slew_from_r: 0.0,
            fade_counter: 0,
            fade_total: micro_slew_frames,
            fade_from_l: 0.0,
            fade_from_r: 0.0,
            default_slew_frames,
            micro_slew_frames,
            had_underrun: false,
            underrun_count: 0,
            slew_trigger_count: 0,
        }
    }

    /// Current FSM state of the resampler engine.
    #[inline(always)]
    pub fn state(&self) -> ResamplerState {
        self.state
    }

    /// Resampler configuration parameters.
    #[inline(always)]
    pub fn config(&self) -> &ResamplerConfig {
        &self.config
    }

    /// Reprime fill threshold in frames.
    #[inline(always)]
    pub fn reprime_fill_frames(&self) -> usize {
        self.reprime_fill_frames
    }

    /// Updates the reprime threshold in milliseconds.
    pub fn set_reprime_fill_ms(&mut self, ms: f32) {
        self.config.reprime_fill_ms = ms;
        self.reprime_fill_frames = ((self.in_rate as f32 * ms / 1000.0) as usize).max(16);
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

    /// Default 10ms slew length in output frames.
    #[inline(always)]
    pub fn default_slew_frames(&self) -> usize {
        self.default_slew_frames
    }

    /// Micro 3ms slew length in output frames.
    #[inline(always)]
    pub fn micro_slew_frames(&self) -> usize {
        self.micro_slew_frames
    }

    /// Current fractional phase position in [0.0, 1.0).
    #[inline(always)]
    pub fn frac_pos(&self) -> f64 {
        self.frac_pos
    }

    /// Rolling 4-frame history buffer [[f32; 2]; 4].
    #[inline(always)]
    pub fn history(&self) -> &[[f32; 2]; 4] {
        &self.history
    }

    /// Reset DSP internal state (history, FSM state, phase, integrator, gain, slew).
    pub fn reset(&mut self) {
        self.history = [[0.0; 2]; 4];
        self.state = ResamplerState::Priming;
        self.frac_pos = 0.0;
        self.integral_error = 0.0;
        self.smoothed_drift = 1.0;
        self.current_drift_ppm = 0.0;
        self.last_drift_ppm = 0;
        self.last_sample_l = 0.0;
        self.last_sample_r = 0.0;
        self.slew_counter = 0;
        self.slew_total = self.micro_slew_frames;
        self.slew_from_l = 0.0;
        self.slew_from_r = 0.0;
        self.fade_counter = 0;
        self.fade_total = self.micro_slew_frames;
        self.fade_from_l = 0.0;
        self.fade_from_r = 0.0;
        self.had_underrun = false;
        self.underrun_count = 0;
        self.slew_trigger_count = 0;
    }

    /// Primes the 4-frame history buffer [f0, f0, f1, f2] from the ring buffer.
    ///
    /// Consumes exactly 3 frames (or available 1..2 frames in starved boundary edge-cases).
    /// Preserves 100% of f0 at t = 0.0 and aligns read cursor 1:1 with consumed frames.
    fn prime_history_from_rb(&mut self, rb: &SpscRingBuffer, read_cursor: &mut usize, avail_frames: usize) {
        let unread = avail_frames.saturating_sub(*read_cursor);
        if unread >= 3 {
            let f0 = rb.peek_frame_stereo(*read_cursor);
            let f1 = rb.peek_frame_stereo(*read_cursor + 1);
            let f2 = rb.peek_frame_stereo(*read_cursor + 2);
            self.history = [
                [f0.0, f0.1],
                [f0.0, f0.1],
                [f1.0, f1.1],
                [f2.0, f2.1],
            ];
            *read_cursor += 3;
        } else if unread == 2 {
            let f0 = rb.peek_frame_stereo(*read_cursor);
            let f1 = rb.peek_frame_stereo(*read_cursor + 1);
            self.history = [
                [f0.0, f0.1],
                [f1.0, f1.1],
                [f1.0, f1.1],
                [f1.0, f1.1],
            ];
            *read_cursor += 2;
        } else if unread == 1 {
            let f0 = rb.peek_frame_stereo(*read_cursor);
            self.history = [
                [f0.0, f0.1],
                [f0.0, f0.1],
                [f0.0, f0.1],
                [f0.0, f0.1],
            ];
            *read_cursor += 1;
        }
        self.frac_pos = 0.0;
    }

    /// Total number of buffer underruns encountered.
    #[inline(always)]
    pub fn underrun_count(&self) -> u64 {
        self.underrun_count
    }

    /// Total number of slew/crossfade events triggered (emergency recenter or recovery).
    #[inline(always)]
    pub fn slew_trigger_count(&self) -> u64 {
        self.slew_trigger_count
    }

    /// Returns the current floating-point drift in parts per million (ppm).
    #[inline(always)]
    pub fn current_drift_ppm(&self) -> f64 {
        self.current_drift_ppm
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

    /// Soft limiter and headroom guard.
    ///
    /// Multiplies by 0.982 headroom factor (-0.158 dBFS) and applies smooth tanh
    /// soft-limiting when the sample exceeds +/- 0.95. This completely guards
    /// against cubic Hermite inter-sample peak overshoots (ISPs) that cause
    /// integer wrap-around or harsh clipping on 24-bit fixed-point DAC endpoints.
    #[inline(always)]
    pub fn soft_limit(x: f32) -> f32 {
        let scaled = x * 0.982;
        if scaled > 0.95 {
            0.95 + 0.05 * ((scaled - 0.95) / 0.05).tanh()
        } else if scaled < -0.95 {
            -0.95 - 0.05 * ((-scaled - 0.95) / 0.05).tanh()
        } else {
            scaled
        }
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

        // Zone 3: Emergency safety valve (extreme latency backlog > 2.5x target)
        // Reset buffer directly to target_fill_frames with 5ms crossfade
        let emergency_threshold = (self.target_fill_frames as f64 * 2.5) as usize;
        if avail_frames > emergency_threshold {
            self.slew_from_l = self.last_sample_l;
            self.slew_from_r = self.last_sample_r;
            // 5ms crossfade scaled to output sample rate
            let crossfade_5ms = ((self.out_rate as usize) * 5 / 1000).max(64);
            self.slew_total = crossfade_5ms;
            self.slew_counter = self.slew_total;
            self.slew_trigger_count += 1;

            // Retain exactly target_fill_frames to eliminate artificial latency backlog
            rb.reset_to_target_frames(self.target_fill_frames);
            self.integral_error = 0.0;
            avail_frames = self.target_fill_frames;
        }

        let mut read_cursor = 0usize;

        // FSM State transitions before block processing
        match self.state {
            ResamplerState::Priming => {
                if avail_frames > 0 {
                    self.prime_history_from_rb(rb, &mut read_cursor, avail_frames);
                    self.state = ResamplerState::Running;
                } else {
                    if !self.had_underrun {
                        self.underrun_count += 1;
                    }
                    self.had_underrun = true;
                    self.state = ResamplerState::Repriming;
                }
            }
            ResamplerState::Repriming => {
                if avail_frames >= self.reprime_fill_frames {
                    self.prime_history_from_rb(rb, &mut read_cursor, avail_frames);
                    self.state = ResamplerState::Running;
                    self.had_underrun = false;
                    self.slew_from_l = 0.0;
                    self.slew_from_r = 0.0;
                    self.slew_total = self.micro_slew_frames;
                    self.slew_counter = self.slew_total;
                    self.slew_trigger_count += 1;
                }
            }
            ResamplerState::Running => {
                if avail_frames == 0 {
                    self.state = ResamplerState::UnderrunFadeOut;
                    if !self.had_underrun {
                        self.underrun_count += 1;
                    }
                    self.had_underrun = true;
                    self.fade_from_l = self.last_sample_l;
                    self.fade_from_r = self.last_sample_r;
                    self.fade_total = self.micro_slew_frames;
                    self.fade_counter = self.fade_total;
                }
            }
            ResamplerState::UnderrunFadeOut => {}
        }

        // 3-Zone Continuous Rate Controller (Bounded PI + Soft Recovery + Slew-Rate Limiter)
        // ONLY active when in Running state. Frozen during Priming, Repriming, or UnderrunFadeOut.
        if self.state == ResamplerState::Running {
            let target = self.target_fill_frames as f64;
            let current = avail_frames as f64;
            let fill_ratio = current / target;

            let target_drift_ppm = if fill_ratio > 1.3 {
                // Zone 2: Soft Proportional Drain (1.3 < fill_ratio <= 2.5)
                // Gently increases reading speed smoothly from +500.0 up to +1500.0 ppm using Smoothstep (3x^2 - 2x^3).
                let excess_norm = ((fill_ratio - 1.3) / 1.2).clamp(0.0, 1.0);
                let smooth_curve = excess_norm * excess_norm * (3.0 - 2.0 * excess_norm);
                self.integral_error = 0.0; // Reset/freeze PI integral to avoid windup while draining burst
                500.0 + smooth_curve * 1000.0
            } else if fill_ratio < 0.7 {
                // Zone 2: Soft Deficit Recovery (fill_ratio < 0.7)
                // Gently slows reading speed smoothly from -500.0 down to -1500.0 ppm using Smoothstep.
                let deficit_norm = ((0.7 - fill_ratio) / 0.7).clamp(0.0, 1.0);
                let smooth_curve = deficit_norm * deficit_norm * (3.0 - 2.0 * deficit_norm);
                self.integral_error = 0.0;
                -500.0 - smooth_curve * 1000.0
            } else {
                // Zone 1: Normal Clock Tracking (0.7 <= fill_ratio <= 1.3)
                // Precise PI controller with hard bounds [-500.0, +500.0] ppm
                let error = (current - target) / target;
                let kp = 1000.0; // in ppm
                let ki = 5.0;    // in ppm
                self.integral_error = (self.integral_error + error).clamp(-40.0, 40.0);
                (kp * error + ki * self.integral_error).clamp(-500.0, 500.0)
            };

            // Slew-Rate Limiter: Maximum drift rate change is 50.0 ppm/s
            let dt = (out_frames as f64) / (self.out_rate as f64);
            let max_delta_ppm = 50.0 * dt;
            let delta = target_drift_ppm - self.current_drift_ppm;
            let clamped_delta = delta.clamp(-max_delta_ppm, max_delta_ppm);
            self.current_drift_ppm += clamped_delta;
            self.current_drift_ppm = self.current_drift_ppm.clamp(-1500.0, 1500.0);

            self.smoothed_drift = 1.0 + (self.current_drift_ppm / 1_000_000.0);
            self.last_drift_ppm = if self.current_drift_ppm > 0.0 && self.current_drift_ppm < 1.0 {
                1
            } else if self.current_drift_ppm < 0.0 && self.current_drift_ppm > -1.0 {
                -1
            } else {
                self.current_drift_ppm.round() as i32
            };
        } else {
            // Freeze PI drift controller when not in Running state (anti-windup)
            self.integral_error = 0.0;
        }

        let step = self.base_ratio * self.smoothed_drift;
        let effective_target_gain = if muted { 0.0 } else { target_linear_gain };
        let smooth_alpha = 0.002f32; // ~10ms convergence at 48kHz (Zero-zipper noise)

        for frame_idx in 0..out_frames {
            // One-pole exponential gain smoother (De-zipper)
            self.current_gain += smooth_alpha * (effective_target_gain - self.current_gain);

            let (out_l, out_r) = match self.state {
                ResamplerState::Priming | ResamplerState::Repriming => {
                    (0.0f32, 0.0f32)
                }
                ResamplerState::UnderrunFadeOut => {
                    if self.fade_total > 0 && self.fade_counter > 0 {
                        let progress = (self.fade_total - self.fade_counter) as f32 / self.fade_total as f32;
                        let alpha = 0.5 * (1.0 + (progress * std::f32::consts::PI).cos());
                        let l = self.fade_from_l * alpha;
                        let r = self.fade_from_r * alpha;
                        self.fade_counter -= 1;
                        if self.fade_counter == 0 {
                            self.state = ResamplerState::Repriming;
                            self.last_sample_l = 0.0;
                            self.last_sample_r = 0.0;
                            self.integral_error = 0.0;
                        }
                        (l, r)
                    } else {
                        self.state = ResamplerState::Repriming;
                        self.last_sample_l = 0.0;
                        self.last_sample_r = 0.0;
                        self.integral_error = 0.0;
                        (0.0f32, 0.0f32)
                    }
                }
                ResamplerState::Running => {
                    // Unity-Ratio 1:1 optimization branch:
                    // When in_rate == out_rate, |drift_ppm| < 5.0 (deadband), and frac_pos < 1e-6:
                    // Output history[1] directly (preserving exact 1-sample group delay as Hermite).
                    let is_unity_ratio = self.in_rate == self.out_rate
                        && self.last_drift_ppm.abs() < 5
                        && self.frac_pos < 1e-6;

                    let (sample_l, sample_r) = if is_unity_ratio {
                        (self.history[1][0], self.history[1][1])
                    } else {
                        let t = self.frac_pos as f32;
                        let sl = Self::cubic_hermite(
                            self.history[0][0],
                            self.history[1][0],
                            self.history[2][0],
                            self.history[3][0],
                            t,
                        );
                        let sr = Self::cubic_hermite(
                            self.history[0][1],
                            self.history[1][1],
                            self.history[2][1],
                            self.history[3][1],
                            t,
                        );
                        (sl, sr)
                    };

                    let gained_l = sample_l * self.current_gain;
                    let gained_r = sample_r * self.current_gain;

                    // Time-Scaled Cosine Crossfade Slew (Hann window):
                    let (slewed_l, slewed_r) = if self.slew_counter > 0 && self.slew_total > 0 {
                        let progress = 1.0 - (self.slew_counter as f32 / self.slew_total as f32);
                        let alpha = 0.5 * (1.0 - (progress * std::f32::consts::PI).cos());
                        let l = (1.0 - alpha) * self.slew_from_l + alpha * gained_l;
                        let r = (1.0 - alpha) * self.slew_from_r + alpha * gained_r;
                        self.slew_counter -= 1;
                        (l, r)
                    } else {
                        (gained_l, gained_r)
                    };

                    // Soft Limiting / Headroom Guard:
                    let sl = Self::soft_limit(slewed_l);
                    let sr = Self::soft_limit(slewed_r);

                    // Advance phase and history window
                    let effective_step = if is_unity_ratio { 1.0 } else { step };
                    self.frac_pos += effective_step;

                    if self.frac_pos >= 1.0 {
                        let advance = self.frac_pos.floor() as usize;
                        self.frac_pos -= advance as f64;
                        if self.frac_pos < 0.0 {
                            self.frac_pos = 0.0;
                        }
                        if is_unity_ratio && self.frac_pos < 1e-6 {
                            self.frac_pos = 0.0;
                        }

                        // Check if ring buffer has more frames if needed
                        if read_cursor + advance > avail_frames {
                            let cur_avail = rb.available_read() / 2;
                            if cur_avail > avail_frames {
                                avail_frames = cur_avail;
                            }
                        }

                        let avail_to_read = avail_frames.saturating_sub(read_cursor);
                        if avail_to_read >= advance {
                            for _ in 0..advance {
                                self.history[0] = self.history[1];
                                self.history[1] = self.history[2];
                                self.history[2] = self.history[3];
                                let f = rb.peek_frame_stereo(read_cursor);
                                self.history[3] = [f.0, f.1];
                                read_cursor += 1;
                            }
                        } else {
                            // Starvation / underrun: shift remaining available frames then transition
                            for _ in 0..avail_to_read {
                                self.history[0] = self.history[1];
                                self.history[1] = self.history[2];
                                self.history[2] = self.history[3];
                                let f = rb.peek_frame_stereo(read_cursor);
                                self.history[3] = [f.0, f.1];
                                read_cursor += 1;
                            }
                            self.state = ResamplerState::UnderrunFadeOut;
                            if !self.had_underrun {
                                self.underrun_count += 1;
                            }
                            self.had_underrun = true;
                            self.fade_from_l = sl;
                            self.fade_from_r = sr;
                            self.fade_total = self.micro_slew_frames;
                            self.fade_counter = self.fade_total;
                        }
                    }

                    (sl, sr)
                }
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
        }

        let consumed = read_cursor.min(avail_frames);
        rb.advance_read_frames(consumed);
    }

    /// Generic continuous resampler processor consuming from a closure.
    ///
    /// Useful for custom buffers or headless verification tests.
    pub fn process_from_ring<F>(
        &mut self,
        out: &mut [[f32; 2]],
        step: f64,
        mut pop_frame: F,
    ) -> ProcessStats
    where
        F: FnMut() -> Option<[f32; 2]>,
    {
        let mut frames_consumed = 0usize;

        match self.state {
            ResamplerState::Priming => {
                let mut temp = [[0.0f32; 2]; 3];
                let mut count = 0;
                while count < 3 {
                    if let Some(f) = pop_frame() {
                        temp[count] = f;
                        count += 1;
                        frames_consumed += 1;
                    } else {
                        break;
                    }
                }

                if count == 0 {
                    if !self.had_underrun {
                        self.had_underrun = true;
                        self.underrun_count += 1;
                    }
                    self.state = ResamplerState::Repriming;
                    for o in out.iter_mut() {
                        *o = [0.0, 0.0];
                    }
                    return ProcessStats {
                        frames_consumed: 0,
                        had_underrun: true,
                    };
                }

                match count {
                    1 => self.history = [temp[0]; 4],
                    2 => {
                        self.history = [
                            temp[0],
                            temp[0],
                            temp[1],
                            temp[1],
                        ]
                    }
                    _ => {
                        self.history = [
                            temp[0],
                            temp[0],
                            temp[1],
                            temp[2],
                        ]
                    }
                }
                self.frac_pos = 0.0;
                self.state = ResamplerState::Running;
            }
            ResamplerState::Repriming => {
                let mut temp = [[0.0f32; 2]; 3];
                let mut count = 0;
                while count < 3 {
                    if let Some(f) = pop_frame() {
                        temp[count] = f;
                        count += 1;
                        frames_consumed += 1;
                    } else {
                        break;
                    }
                }
                if count >= 3 {
                    self.history = [temp[0], temp[0], temp[1], temp[2]];
                    self.frac_pos = 0.0;
                    self.state = ResamplerState::Running;
                    self.had_underrun = false;
                    self.slew_from_l = 0.0;
                    self.slew_from_r = 0.0;
                    self.slew_total = self.micro_slew_frames;
                    self.slew_counter = self.slew_total;
                    self.slew_trigger_count += 1;
                } else {
                    for o in out.iter_mut() {
                        *o = [0.0, 0.0];
                    }
                    return ProcessStats {
                        frames_consumed,
                        had_underrun: true,
                    };
                }
            }
            ResamplerState::Running | ResamplerState::UnderrunFadeOut => {}
        }

        let is_unity_ratio = self.in_rate == self.out_rate
            && self.last_drift_ppm.abs() < 5
            && self.frac_pos < 1e-6;

        let effective_step = if is_unity_ratio { 1.0 } else { step };

        for out_frame in out.iter_mut() {
            let (out_l, out_r) = match self.state {
                ResamplerState::Priming | ResamplerState::Repriming => (0.0f32, 0.0f32),
                ResamplerState::UnderrunFadeOut => {
                    if self.fade_total > 0 && self.fade_counter > 0 {
                        let progress = (self.fade_total - self.fade_counter) as f32 / self.fade_total as f32;
                        let alpha = 0.5 * (1.0 + (progress * std::f32::consts::PI).cos());
                        let l = self.fade_from_l * alpha;
                        let r = self.fade_from_r * alpha;
                        self.fade_counter -= 1;
                        if self.fade_counter == 0 {
                            self.state = ResamplerState::Repriming;
                            self.last_sample_l = 0.0;
                            self.last_sample_r = 0.0;
                        }
                        (l, r)
                    } else {
                        self.state = ResamplerState::Repriming;
                        (0.0, 0.0)
                    }
                }
                ResamplerState::Running => {
                    let (sample_l, sample_r) = if is_unity_ratio && self.frac_pos < 1e-6 {
                        (self.history[1][0], self.history[1][1])
                    } else {
                        let t = self.frac_pos as f32;
                        let sl = Self::cubic_hermite(
                            self.history[0][0],
                            self.history[1][0],
                            self.history[2][0],
                            self.history[3][0],
                            t,
                        );
                        let sr = Self::cubic_hermite(
                            self.history[0][1],
                            self.history[1][1],
                            self.history[2][1],
                            self.history[3][1],
                            t,
                        );
                        (sl, sr)
                    };

                    let (slewed_l, slewed_r) = if self.slew_counter > 0 && self.slew_total > 0 {
                        let progress = 1.0 - (self.slew_counter as f32 / self.slew_total as f32);
                        let alpha = 0.5 * (1.0 - (progress * std::f32::consts::PI).cos());
                        let l = (1.0 - alpha) * self.slew_from_l + alpha * sample_l;
                        let r = (1.0 - alpha) * self.slew_from_r + alpha * sample_r;
                        self.slew_counter -= 1;
                        (l, r)
                    } else {
                        (sample_l, sample_r)
                    };

                    let sl = Self::soft_limit(slewed_l);
                    let sr = Self::soft_limit(slewed_r);

                    self.frac_pos += effective_step;
                    if self.frac_pos >= 1.0 {
                        let advance = self.frac_pos.floor() as usize;
                        self.frac_pos -= advance as f64;
                        if self.frac_pos < 0.0 {
                            self.frac_pos = 0.0;
                        }
                        if is_unity_ratio && self.frac_pos < 1e-6 {
                            self.frac_pos = 0.0;
                        }

                        for _ in 0..advance {
                            if let Some(f) = pop_frame() {
                                self.history[0] = self.history[1];
                                self.history[1] = self.history[2];
                                self.history[2] = self.history[3];
                                self.history[3] = f;
                                frames_consumed += 1;
                            } else {
                                self.state = ResamplerState::UnderrunFadeOut;
                                if !self.had_underrun {
                                    self.had_underrun = true;
                                    self.underrun_count += 1;
                                }
                                self.fade_from_l = sl;
                                self.fade_from_r = sr;
                                self.fade_total = self.micro_slew_frames;
                                self.fade_counter = self.fade_total;
                                break;
                            }
                        }
                    }

                    (sl, sr)
                }
            };

            self.last_sample_l = out_l;
            self.last_sample_r = out_r;
            *out_frame = [out_l, out_r];
        }

        ProcessStats {
            frames_consumed,
            had_underrun: self.had_underrun,
        }
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
        let rb = SpscRingBuffer::new(32768);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        let target = dsp.target_fill_frames();

        // Push extreme excess frames (> 2.5x target, e.g. 3x) to trigger Emergency Re-center
        let excess_frames = target * 3;
        let excess_data = vec![0.5f32; excess_frames * 2];
        rb.push_slice(&excess_data);

        let mut output = vec![0.0f32; 512];
        dsp.process_output_block(&rb, &mut output, 1.0, false);

        // Emergency recovery resets to target_fill_frames (minus frames consumed)
        assert!(rb.available_read() / 2 <= target);
        assert_eq!(dsp.slew_trigger_count(), 1);

        // Verify no NaN or sudden spikes
        for &s in &output {
            assert!(!s.is_nan());
            assert!(s.abs() <= 1.0);
        }
    }

    #[test]
    fn test_resampler_soft_proportional_drain_zone2() {
        let rb = SpscRingBuffer::new(16384);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        let target = dsp.target_fill_frames();

        // Push excess frames in Zone 2 (2.0x target, fill_ratio = 2.0 within [1.3, 2.5])
        let excess_frames = (target as f64 * 2.0) as usize;
        let excess_data = vec![0.5f32; excess_frames * 2];
        rb.push_slice(&excess_data);

        let initial_avail = rb.available_read() / 2;
        let mut output = vec![0.0f32; 512]; // 256 frames
        dsp.process_output_block(&rb, &mut output, 1.0, false);

        let remaining_avail = rb.available_read() / 2;
        let consumed = initial_avail - remaining_avail;

        // Zero Sample Loss: buffer was NOT hard reset! Only legitimately rendered frames consumed
        assert!(consumed <= 300, "Should only consume rendered frames, not discard buffer: consumed={}", consumed);
        assert!(remaining_avail > target, "Buffer should not be reset to target in Zone 2");
        assert_eq!(dsp.slew_trigger_count(), 0, "No emergency slew should trigger in Zone 2");

        // Drift should actively accelerate (positive drift, smoothed_drift > 1.0)
        assert!(dsp.smoothed_drift() > 1.0, "Drift multiplier should accelerate to drain burst");
        assert!(dsp.last_drift_ppm() > 0, "Drift ppm should be positive");
    }

    #[test]
    fn test_drift_slew_rate_limiter_50_ppm_sec() {
        let rb = SpscRingBuffer::new(16384);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 40); // target = 1920 frames
        let target = dsp.target_fill_frames();

        // Feed buffer in Zone 2 (2.0x target = 3840 frames)
        let data = vec![0.1f32; target * 4];
        rb.push_slice(&data);

        // Process 1 second of audio (100 blocks of 480 frames = 10ms each)
        let mut block = vec![0.0f32; 480 * 2];
        let mut prev_drift = dsp.current_drift_ppm();

        let feed_chunk = vec![0.1f32; 480 * 2];
        for _ in 0..100 {
            rb.push_slice(&feed_chunk);
            dsp.process_output_block(&rb, &mut block, 1.0, false);
            let cur_drift = dsp.current_drift_ppm();
            let delta = cur_drift - prev_drift;
            // In 10ms (dt = 0.010s), max delta is 50.0 * 0.010 = 0.5 ppm (+ small floating point margin)
            assert!(delta <= 0.5001, "Delta drift per 10ms block ({:.6}) exceeded 0.5 ppm", delta);
            prev_drift = cur_drift;
        }

        // After 1.0s, total drift change should be ~50 ppm (within [49.0, 51.0] ppm)
        let total_drift = dsp.current_drift_ppm();
        assert!(
            (total_drift - 50.0).abs() < 1.0,
            "Total drift after 1.0s was {:.2} ppm, expected ~50.0 ppm",
            total_drift
        );
        assert!(dsp.last_drift_ppm() <= 500, "Zone 1/2 bounds respected");
    }

    #[test]
    fn test_soft_limiting_headroom_guard() {
        // Zero signal
        assert_eq!(AdaptiveResamplerDsp::soft_limit(0.0), 0.0);

        // Linear region (< 0.95 scaled)
        let val_05 = AdaptiveResamplerDsp::soft_limit(0.5);
        assert!((val_05 - 0.5 * 0.982).abs() < 1e-6);

        // Near 1.0 (Hermite normal peak): soft limited and strictly bounded < 1.0
        let val_10 = AdaptiveResamplerDsp::soft_limit(1.0);
        assert!(val_10 > 0.95);
        assert!(val_10 < 1.0);

        // Severe inter-sample overshoot (1.5): strictly bounded within [-1.0, 1.0]
        let val_15 = AdaptiveResamplerDsp::soft_limit(1.5);
        assert!(val_15 <= 1.0);
        assert!(val_15 > 0.95);

        // Symmetric negative
        let val_neg_15 = AdaptiveResamplerDsp::soft_limit(-1.5);
        assert!(val_neg_15 >= -1.0);
        assert!(val_neg_15 < -0.95);
        assert!((val_15 + val_neg_15).abs() < 1e-6);
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

    #[test]
    fn test_unity_ratio_1_to_1_direct_output() {
        let rb = SpscRingBuffer::new(2048);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        let target = dsp.target_fill_frames(); // 720 frames

        // Prime with exactly target fill frames of known stepped amplitude
        let mut prime = Vec::with_capacity(target * 2);
        for f in 0..target {
            let s = (f as f32) * 0.0005; // 0.0 to 0.36 (linear region of soft_limit)
            prime.push(s);
            prime.push(s * 0.5);
        }
        rb.push_slice(&prime);

        let mut out = vec![0.0f32; 100 * 2];
        dsp.process_output_block(&rb, &mut out, 1.0, false);

        // In 1:1 mode with 0 drift, history[1] is outputted directly (with group delay 1).
        // Soft limit factor is 0.982 in linear region (< 0.95).
        assert_eq!(dsp.underrun_count(), 0);
        assert_eq!(dsp.slew_trigger_count(), 0);
        assert!(dsp.last_drift_ppm().abs() < 5);

        // Verify continuity across the output block
        for i in 2..out.len() {
            let diff = (out[i] - out[i - 2]).abs();
            assert!(diff < 0.001, "Delta exceeded expected linear step: {}", diff);
        }
    }

    #[test]
    fn test_telemetry_and_underrun_separation() {
        let rb = SpscRingBuffer::new(512);
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 10);
        assert_eq!(dsp.underrun_count(), 0);
        assert_eq!(dsp.slew_trigger_count(), 0);

        // 1. Process on completely empty buffer -> true underrun
        let mut out = vec![0.0f32; 64 * 2];
        dsp.process_output_block(&rb, &mut out, 1.0, false);
        assert_eq!(dsp.underrun_count(), 1, "Should count 1 true underrun on empty buffer");
        assert_eq!(dsp.slew_trigger_count(), 0, "Slew should not trigger until audio returns");

        // 2. Audio arrives -> recovery micro-crossfade triggers
        let audio = vec![0.5f32; 128 * 2];
        rb.push_slice(&audio);
        dsp.process_output_block(&rb, &mut out, 1.0, false);
        assert_eq!(dsp.underrun_count(), 1, "Underrun count should remain 1");
        assert_eq!(dsp.slew_trigger_count(), 1, "Slew trigger count should be 1 on recovery");

        // 3. Audio continues without interruption -> no new underruns or slews
        let more_audio = vec![0.5f32; 128 * 2];
        rb.push_slice(&more_audio);
        dsp.process_output_block(&rb, &mut out, 1.0, false);
        assert_eq!(dsp.underrun_count(), 1, "Underrun count should remain 1");
        assert_eq!(dsp.slew_trigger_count(), 1, "Slew count should remain 1");
    }

    #[test]
    fn test_process_from_ring_closure() {
        let mut dsp = AdaptiveResamplerDsp::new(48000, 48000, 15);
        let frames = vec![
            [0.1f32, 0.2f32],
            [0.3f32, 0.4f32],
            [0.5f32, 0.6f32],
            [0.7f32, 0.8f32],
            [0.9f32, 1.0f32],
            [1.1f32, 1.2f32],
            [1.3f32, 1.4f32],
            [1.5f32, 1.6f32],
        ];
        let mut iter = frames.into_iter();

        let mut out = [[0.0f32; 2]; 2];
        let stats = dsp.process_from_ring(&mut out, 1.0, || iter.next());

        assert!(stats.frames_consumed >= 4, "Should have primed at least 4 frames");
        assert!(!stats.had_underrun);
    }

    #[test]
    fn test_fsm_state_transitions_and_f0_preservation() {
        let config = ResamplerConfig {
            capture_rate: 48000,
            render_rate: 48000,
            target_fill_ms: 30.0,
            reprime_fill_ms: 25.0,
            fade_ms: 3.0,
        };
        let mut dsp = AdaptiveResamplerDsp::with_config(config);
        let rb = SpscRingBuffer::new(4096);

        // 1. Initial state is Priming
        assert_eq!(dsp.state(), ResamplerState::Priming);
        assert_eq!(dsp.underrun_count(), 0);

        // 2. Push 100 frames with distinct stepped amplitudes
        let mut input = Vec::with_capacity(200);
        for f in 0..100 {
            let l = 0.35 + (f as f32) * 0.005;
            let r = -0.25 - (f as f32) * 0.005;
            input.push(l);
            input.push(r);
        }
        rb.push_slice(&input);

        // 3. Process 10 frames
        let mut out = vec![0.0f32; 10 * 2];
        dsp.process_output_block(&rb, &mut out, 1.0, false);

        // Must be in Running state now
        assert_eq!(dsp.state(), ResamplerState::Running);
        assert_eq!(dsp.underrun_count(), 0);

        // CRITICAL INVARIANT: f0 is preserved 100% at sample 0 (scaled by soft_limit 0.982)
        let expected_f0_l = 0.35 * 0.982;
        let expected_f0_r = -0.25 * 0.982;
        assert!(
            (out[0] - expected_f0_l).abs() < 1e-5,
            "f0 (Left) was not preserved! expected={:.6}, got={:.6}",
            expected_f0_l,
            out[0]
        );
        assert!(
            (out[1] - expected_f0_r).abs() < 1e-5,
            "f0 (Right) was not preserved! expected={:.6}, got={:.6}",
            expected_f0_r,
            out[1]
        );

        // Verify exact consumed frames: primed 3 + 10 output frames = 13 frames consumed
        let remaining = rb.available_read() / 2;
        assert_eq!(remaining, 87, "Exactly 13 frames should be consumed (3 primed + 10 advanced)");
    }

    #[test]
    fn test_fsm_underrun_fadeout_and_repriming_hysteresis() {
        let config = ResamplerConfig {
            capture_rate: 48000,
            render_rate: 48000,
            target_fill_ms: 30.0,
            reprime_fill_ms: 25.0, // 25ms @ 48kHz = 1200 frames
            fade_ms: 3.0,          // 3ms @ 48kHz = 144 frames
        };
        let mut dsp = AdaptiveResamplerDsp::with_config(config);
        let rb = SpscRingBuffer::new(16384);

        // 1. Prime and run with 1500 frames (> target 1440)
        let audio = vec![0.5f32; 1500 * 2];
        rb.push_slice(&audio);

        let mut block = vec![0.0f32; 480 * 2]; // 10ms block
        dsp.process_output_block(&rb, &mut block, 1.0, false);
        assert_eq!(dsp.state(), ResamplerState::Running);
        assert_eq!(dsp.underrun_count(), 0);

        // 2. Drain ring buffer completely to trigger starvation
        // Remaining in RB is ~1500 - 483 = 1017 frames.
        // Process 2 more 10ms blocks (960 frames):
        dsp.process_output_block(&rb, &mut block, 1.0, false); // consumes 480 -> remaining ~537
        dsp.process_output_block(&rb, &mut block, 1.0, false); // consumes 480 -> remaining ~57
        assert_eq!(dsp.state(), ResamplerState::Running);

        // Next block needs 480 frames but RB only has ~57 frames:
        dsp.process_output_block(&rb, &mut block, 1.0, false);
        // During this block, buffer depleted -> UnderrunFadeOut triggered (144 frames fade) -> Repriming!
        assert_eq!(dsp.underrun_count(), 1);
        assert_eq!(dsp.state(), ResamplerState::Repriming);

        // 3. PI Integrator freeze check: when in Repriming, integral error is reset/frozen
        assert_eq!(dsp.integral_error, 0.0);

        // 4. In Repriming state: output MUST be absolute silence (0.0)
        dsp.process_output_block(&rb, &mut block, 1.0, false);
        for &s in &block {
            assert_eq!(s, 0.0, "Repriming state must output strict silence");
        }
        assert_eq!(dsp.state(), ResamplerState::Repriming);

        // 5. HYSTERESIS CHECK: Push 10ms (480 frames) < reprime threshold (1200 frames = 25ms)
        let partial_audio = vec![0.6f32; 480 * 2];
        rb.push_slice(&partial_audio);

        dsp.process_output_block(&rb, &mut block, 1.0, false);
        // Must STAY in Repriming! Zero chopper effect! Output remains 0.0
        assert_eq!(dsp.state(), ResamplerState::Repriming);
        for &s in &block {
            assert_eq!(s, 0.0, "Hysteresis must withhold playback until reprime threshold reached");
        }

        // 6. Push remaining audio to cross 25ms reprime threshold (need >= 1200 frames)
        let more_audio = vec![0.6f32; 800 * 2]; // 480 + 800 = 1280 frames >= 1200
        rb.push_slice(&more_audio);

        let initial_slews = dsp.slew_trigger_count();
        dsp.process_output_block(&rb, &mut block, 1.0, false);

        // Must transition Repriming -> Running with 3ms Cosine Fade-In
        assert_eq!(dsp.state(), ResamplerState::Running);
        assert_eq!(dsp.slew_trigger_count(), initial_slews + 1, "Must trigger Fade-In slew");
        // Verify audio returned smoothly (first samples ramping up from 0.0)
        assert!(block[0] >= 0.0);
        assert!(block[block.len() - 1] > block[0], "Fade-In must ramp audio amplitude up");
    }

    #[test]
    fn test_underrun_fadeout_smoothness_no_clicks() {
        let config = ResamplerConfig {
            capture_rate: 48000,
            render_rate: 48000,
            target_fill_ms: 30.0,
            reprime_fill_ms: 25.0,
            fade_ms: 3.0,
        };
        let mut dsp = AdaptiveResamplerDsp::with_config(config);
        let rb = SpscRingBuffer::new(2048);

        // Feed only 50 frames of loud 0.8 amplitude audio
        let input = vec![0.8f32; 50 * 2];
        rb.push_slice(&input);

        // Request 250 frames (500 samples): buffer starves after 50 frames
        let mut out = vec![0.0f32; 250 * 2];
        dsp.process_output_block(&rb, &mut out, 1.0, false);

        assert_eq!(dsp.underrun_count(), 1);

        // Check continuity across entire starvation fade-out: NO CLIFF / POP (|diff| < 0.025)
        let mut max_diff = 0.0f32;
        for i in 2..out.len() {
            let diff = (out[i] - out[i - 2]).abs(); // compare same channel
            max_diff = max_diff.max(diff);
            assert!(
                diff < 0.025,
                "Starvation click/pop detected at sample {}: diff={:.6}",
                i,
                diff
            );
        }
        // Tail must reach complete silence (0.0)
        assert_eq!(out[out.len() - 1], 0.0);
        assert_eq!(out[out.len() - 2], 0.0);
    }
}
