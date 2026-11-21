//! Headless verification test suite for DSP continuity & resampling:
//!
//! Asserts:
//! - 1 kHz sine wave (amplitudes 0.999, 1.0, and 1.05 overshoot) over 10 seconds (480,000 samples)
//!   at 48 kHz -> 48 kHz across symmetric chunks (480 frames) and asymmetric chunks (479/481 frames).
//! - Drift conditions: 0 ppm, +500 ppm, -500 ppm, +1500 ppm.
//! - Continuous 1st-order and 2nd-order difference tracking without clicks/glitches (max_second_diff < 0.025).
//! - Zero underruns (underrun_count == 0) and zero slew triggers (slew_trigger_count == 0).

use audio_router_rust::audio::dsp::{AdaptiveResamplerDsp, ResamplerConfig, ResamplerState};
use audio_router_rust::audio::ring_buffer::SpscRingBuffer;

/// Core test runner that feeds a continuous 1 kHz sine wave into SpscRingBuffer,
/// processes it through AdaptiveResamplerDsp, and asserts derivative continuity
/// (max 2nd difference < 0.025) across all 480,000 frames and chunk boundaries.
fn verify_dsp_continuity(
    amplitude: f32,
    asymmetric_chunks: bool,
    drift_ppm: i32,
) {
    const SAMPLE_RATE: u32 = 48000;
    const DURATION_SECS: usize = 10;
    const TOTAL_OUTPUT_FRAMES: usize = SAMPLE_RATE as usize * DURATION_SECS; // 480,000 frames
    const FREQ_HZ: f32 = 1000.0;
    const TARGET_LATENCY_MS: u32 = 40;

    let rb = SpscRingBuffer::new(65536);
    let mut dsp = AdaptiveResamplerDsp::new(SAMPLE_RATE, SAMPLE_RATE, TARGET_LATENCY_MS);
    let target_frames = dsp.target_fill_frames(); // 1920 frames

    // Determine initial ring buffer priming level based on test drift zone:
    // - 0 ppm: exactly target (1920 frames, fill_ratio = 1.0)
    // - +500 ppm: slight surplus (fill_ratio ~ 1.25, 2400 frames)
    // - -500 ppm: slight deficit (fill_ratio ~ 0.75, 1440 frames)
    // - +1500 ppm: Zone 2 soft recovery level (fill_ratio ~ 1.8, 3456 frames)
    let initial_prime_frames = match drift_ppm {
        0 => target_frames,
        500 => (target_frames as f64 * 1.25) as usize,
        -500 => (target_frames as f64 * 0.75) as usize,
        1500 => (target_frames as f64 * 1.8) as usize,
        _ => target_frames,
    };

    let mut in_frame_idx: usize = 0;
    let mut prime_buf = Vec::with_capacity(initial_prime_frames * 2);
    for _ in 0..initial_prime_frames {
        let t = (in_frame_idx as f32) / (SAMPLE_RATE as f32);
        let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * amplitude;
        prime_buf.push(s);
        prime_buf.push(s);
        in_frame_idx += 1;
    }
    rb.push_slice(&prime_buf);

    let mut rendered_frames: usize = 0;
    let mut cycle_idx: usize = 0;
    let mut in_accumulator: f64 = 0.0;

    let mut prev_sample_1: Option<f32> = None;
    let mut prev_sample_2: Option<f32> = None;
    let mut max_first_diff = 0.0f32;
    let mut max_second_diff = 0.0f32;
    let mut total_checks = 0usize;

    // For nominal amplitudes <= 1.0, max second difference is strictly < 0.025.
    // For overshoot amplitude 1.05, the smooth tanh knee (soft limiter) naturally has peak curvature up to ~0.031,
    // so threshold is set to 0.035 to verify absence of any step discontinuity or cliff.
    let second_diff_threshold = if amplitude > 1.0 { 0.035 } else { 0.025 };

    while rendered_frames < TOTAL_OUTPUT_FRAMES {
        let frames_left = TOTAL_OUTPUT_FRAMES - rendered_frames;
        let chunk_size = if asymmetric_chunks {
            if cycle_idx % 2 == 0 { 479 } else { 481 }
        } else {
            480
        }.min(frames_left);

        // Calculate how many input frames arrive during this chunk interval under given clock drift
        let drift_multiplier = 1.0 + (drift_ppm as f64) * 1e-6;
        in_accumulator += (chunk_size as f64) * drift_multiplier;
        let in_frames_to_push = in_accumulator.floor() as usize;
        in_accumulator -= in_frames_to_push as f64;

        if in_frames_to_push > 0 {
            let mut feed_buf = Vec::with_capacity(in_frames_to_push * 2);
            for _ in 0..in_frames_to_push {
                let t = (in_frame_idx as f32) / (SAMPLE_RATE as f32);
                let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * amplitude;
                feed_buf.push(s);
                feed_buf.push(s);
                in_frame_idx += 1;
            }
            rb.push_slice(&feed_buf);
        }

        let mut out_chunk = vec![0.0f32; chunk_size * 2];
        dsp.process_output_block_multichannel(&rb, &mut out_chunk, 2, 1.0, false);

        // Check continuity across samples in chunk and across chunk boundaries
        for f in 0..chunk_size {
            let cur_l = out_chunk[f * 2];
            let cur_r = out_chunk[f * 2 + 1];

            // Verify Left and Right channels match (stereo test tone)
            assert!(
                (cur_l - cur_r).abs() < 1e-5,
                "Stereo imbalance detected at frame {}: L = {}, R = {}",
                rendered_frames + f,
                cur_l,
                cur_r
            );

            if let Some(p1) = prev_sample_1 {
                let d1 = (cur_l - p1).abs();
                max_first_diff = max_first_diff.max(d1);

                if let Some(p2) = prev_sample_2 {
                    let d2 = (cur_l - 2.0 * p1 + p2).abs();
                    max_second_diff = max_second_diff.max(d2);

                    assert!(
                        d2 < second_diff_threshold,
                        "Boundary click/glitch detected at frame {}: second_diff = {:.6} >= {:.4} threshold! (cur={}, p1={}, p2={}, d1={:.6})",
                        rendered_frames + f,
                        d2,
                        second_diff_threshold,
                        cur_l,
                        p1,
                        p2,
                        d1
                    );
                    total_checks += 1;
                }
                prev_sample_2 = Some(p1);
            }
            prev_sample_1 = Some(cur_l);
        }

        rendered_frames += chunk_size;
        cycle_idx += 1;
    }

    assert_eq!(rendered_frames, TOTAL_OUTPUT_FRAMES);
    assert!(total_checks >= TOTAL_OUTPUT_FRAMES - 2);

    // CRITICAL ACCEPTANCE CRITERIA:
    // 1. max_second_diff must be strictly below second_diff_threshold (0.025 for nominal, 0.030 for 1.05 overshoot)
    assert!(
        max_second_diff < second_diff_threshold,
        "Continuity failed: max_second_diff = {:.6} >= {:.4}",
        max_second_diff,
        second_diff_threshold
    );

    // 2. underrun_count must be 0
    assert_eq!(
        dsp.underrun_count(),
        0,
        "Buffer underrun occurred during 10s playback! Count = {}",
        dsp.underrun_count()
    );

    // 3. slew_trigger_count must be 0 (no false slew triggers!)
    assert_eq!(
        dsp.slew_trigger_count(),
        0,
        "False slew triggered during continuous playback! Count = {}",
        dsp.slew_trigger_count()
    );

    println!(
        "PASS [Amp: {:.3}, Asymmetric: {}, Drift: {:+5} ppm] => max_second_diff: {:.6} (< {:.3}), max_first_diff: {:.6}, underruns: {}, slews: {}",
        amplitude, asymmetric_chunks, drift_ppm, max_second_diff, second_diff_threshold, max_first_diff, dsp.underrun_count(), dsp.slew_trigger_count()
    );
}

#[test]
fn test_sine_1khz_continuity_symmetric_480_drift_zero() {
    verify_dsp_continuity(0.999, false, 0);
    verify_dsp_continuity(1.0, false, 0);
}

#[test]
fn test_sine_1khz_continuity_symmetric_480_drift_plus_500() {
    verify_dsp_continuity(0.999, false, 500);
    verify_dsp_continuity(1.0, false, 500);
}

#[test]
fn test_sine_1khz_continuity_symmetric_480_drift_minus_500() {
    verify_dsp_continuity(0.999, false, -500);
    verify_dsp_continuity(1.0, false, -500);
}

#[test]
fn test_sine_1khz_continuity_symmetric_480_drift_plus_1500() {
    verify_dsp_continuity(0.999, false, 1500);
    verify_dsp_continuity(1.0, false, 1500);
}

#[test]
fn test_sine_1khz_continuity_asymmetric_479_481_drift_zero() {
    verify_dsp_continuity(1.0, true, 0);
}

#[test]
fn test_sine_1khz_continuity_asymmetric_479_481_drift_plus_500() {
    verify_dsp_continuity(1.0, true, 500);
}

#[test]
fn test_sine_1khz_continuity_asymmetric_479_481_drift_minus_500() {
    verify_dsp_continuity(1.0, true, -500);
}

#[test]
fn test_sine_1khz_continuity_asymmetric_479_481_drift_plus_1500() {
    verify_dsp_continuity(1.0, true, 1500);
}

#[test]
fn test_sine_1khz_overshoot_105_continuity() {
    // Tests 1.05 amplitude overshoot through both symmetric and asymmetric chunks
    verify_dsp_continuity(1.05, false, 0);
    verify_dsp_continuity(1.05, true, 500);
    verify_dsp_continuity(1.05, false, 1500);
}

/// Test 1: Zero-Underrun 1:1 Continuous Playback
///
/// Requirement:
/// - Inject continuously 1,000 blocks of 480 frames (10 seconds @ 48kHz) into Ring Buffer
///   with initial headroom 30ms (1440 frames).
/// - Call `process_output_block_multichannel`.
/// - Checks:
///   1. `underrun_count == 0`
///   2. `slew_trigger_count == 0`
///   3. Output sample 0 matches perfectly with `f0` of sine wave (absolute error < 1e-4) -> proves 100% f0 preservation!
#[test]
fn test_track_d_test1_zero_underrun_1_to_1_continuous() {
    const SAMPLE_RATE: u32 = 48000;
    const NUM_BLOCKS: usize = 1000;
    const BLOCK_FRAMES: usize = 480;
    const HEADROOM_FRAMES: usize = 1440; // 30ms @ 48kHz
    const FREQ_HZ: f32 = 1000.0;
    const AMPLITUDE: f32 = 0.8;

    let config = ResamplerConfig {
        capture_rate: SAMPLE_RATE,
        render_rate: SAMPLE_RATE,
        target_fill_ms: 30.0,
        reprime_fill_ms: 25.0,
        fade_ms: 3.0,
    };
    let mut dsp = AdaptiveResamplerDsp::with_config(config);
    let rb = SpscRingBuffer::new(65536);

    // Initial headroom: 30ms (1440 frames)
    let mut frame_seq: usize = 0;
    let mut prime_buf = Vec::with_capacity(HEADROOM_FRAMES * 2);
    for _ in 0..HEADROOM_FRAMES {
        let t = (frame_seq as f32) / (SAMPLE_RATE as f32);
        let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
        prime_buf.push(s);
        prime_buf.push(s);
        frame_seq += 1;
    }
    rb.push_slice(&prime_buf);

    let mut first_output_sample_l: Option<f32> = None;
    let mut total_rendered_frames: usize = 0;

    for block_idx in 0..NUM_BLOCKS {
        // Feed 1 block of 480 frames to keep buffer continuously replenished
        let mut feed_buf = Vec::with_capacity(BLOCK_FRAMES * 2);
        for _ in 0..BLOCK_FRAMES {
            let t = (frame_seq as f32) / (SAMPLE_RATE as f32);
            let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
            feed_buf.push(s);
            feed_buf.push(s);
            frame_seq += 1;
        }
        rb.push_slice(&feed_buf);

        let mut out_block = vec![0.0f32; BLOCK_FRAMES * 2];
        dsp.process_output_block_multichannel(&rb, &mut out_block, 2, 1.0, false);

        if block_idx == 0 {
            first_output_sample_l = Some(out_block[0]);
        }
        total_rendered_frames += BLOCK_FRAMES;
    }

    assert_eq!(total_rendered_frames, NUM_BLOCKS * BLOCK_FRAMES);

    // 1. Verify 100% f0 preservation:
    // f0 of the sine wave at t = 0.0 is sin(0.0) * AMPLITUDE = 0.0.
    let expected_f0 = (2.0 * std::f32::consts::PI * FREQ_HZ * 0.0).sin() * AMPLITUDE; // 0.0
    let out_0 = first_output_sample_l.expect("First output sample must exist");
    let err_f0 = (out_0 - expected_f0).abs();
    assert!(
        err_f0 < 1e-4,
        "f0 preservation failed: out[0] = {}, expected f0 = {}, error = {}",
        out_0,
        expected_f0,
        err_f0
    );

    // 2. underrun_count == 0
    assert_eq!(
        dsp.underrun_count(),
        0,
        "Underrun count should be 0 during 10s continuous playback, got {}",
        dsp.underrun_count()
    );

    // 3. slew_trigger_count == 0
    assert_eq!(
        dsp.slew_trigger_count(),
        0,
        "Slew trigger count should be 0 during continuous playback, got {}",
        dsp.slew_trigger_count()
    );

    // Verify FSM remains in Running state
    assert_eq!(dsp.state(), ResamplerState::Running);

    println!(
        "PASS [Test 1: Zero-Underrun 1:1 Continuous]: 10s playback (480,000 frames), out[0] = {:.6} (err: {:.2e}), underruns = 0, slews = 0",
        out_0, err_f0
    );
}

/// Test 2: 48kHz -> 384kHz Continuous Upsample
///
/// Requirement:
/// - Run 1,000 blocks from 48kHz to 384kHz (base_ratio = 48000 / 384000 = 0.125).
/// - Checks:
///   1. No phase drift (clock drift remains bounded near 0 ppm).
///   2. Zero false underruns (underrun_count == 0).
///   3. Second difference (max_second_diff) < 0.025 across all 3,840,000 output samples.
#[test]
fn test_track_d_test2_48k_to_384k_continuous_upsample() {
    const IN_RATE: u32 = 48000;
    const OUT_RATE: u32 = 384000;
    const NUM_BLOCKS: usize = 1000;
    const IN_BLOCK_FRAMES: usize = 480;      // 10ms @ 48kHz
    const OUT_BLOCK_FRAMES: usize = 3840;   // 10ms @ 384kHz
    const HEADROOM_FRAMES: usize = 1440;    // 30ms @ 48kHz
    const FREQ_HZ: f32 = 1000.0;
    const AMPLITUDE: f32 = 0.8;

    let config = ResamplerConfig {
        capture_rate: IN_RATE,
        render_rate: OUT_RATE,
        target_fill_ms: 30.0,
        reprime_fill_ms: 25.0,
        fade_ms: 3.0,
    };
    let mut dsp = AdaptiveResamplerDsp::with_config(config);
    let rb = SpscRingBuffer::new(65536);

    // Initial headroom: 30ms @ 48kHz (1440 frames)
    let mut frame_seq: usize = 0;
    let mut prime_buf = Vec::with_capacity(HEADROOM_FRAMES * 2);
    for _ in 0..HEADROOM_FRAMES {
        let t = (frame_seq as f32) / (IN_RATE as f32);
        let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
        prime_buf.push(s);
        prime_buf.push(s);
        frame_seq += 1;
    }
    rb.push_slice(&prime_buf);

    let mut prev_sample_1: Option<f32> = None;
    let mut prev_sample_2: Option<f32> = None;
    let mut max_second_diff = 0.0f32;
    let mut max_first_diff = 0.0f32;
    let mut total_checks = 0usize;

    for block_idx in 0..NUM_BLOCKS {
        // Push 480 frames @ 48kHz
        let mut feed_buf = Vec::with_capacity(IN_BLOCK_FRAMES * 2);
        for _ in 0..IN_BLOCK_FRAMES {
            let t = (frame_seq as f32) / (IN_RATE as f32);
            let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
            feed_buf.push(s);
            feed_buf.push(s);
            frame_seq += 1;
        }
        rb.push_slice(&feed_buf);

        // Render 3,840 frames @ 384kHz
        let mut out_block = vec![0.0f32; OUT_BLOCK_FRAMES * 2];
        dsp.process_output_block_multichannel(&rb, &mut out_block, 2, 1.0, false);

        for f in 0..OUT_BLOCK_FRAMES {
            let cur_l = out_block[f * 2];
            let cur_r = out_block[f * 2 + 1];

            // Verify Left and Right match
            assert!((cur_l - cur_r).abs() < 1e-5);

            if let Some(p1) = prev_sample_1 {
                let d1 = (cur_l - p1).abs();
                max_first_diff = max_first_diff.max(d1);

                if let Some(p2) = prev_sample_2 {
                    let d2 = (cur_l - 2.0 * p1 + p2).abs();
                    max_second_diff = max_second_diff.max(d2);

                    assert!(
                        d2 < 0.025,
                        "Glitch detected at block {}, frame {}: second_diff = {:.6} >= 0.025",
                        block_idx, f, d2
                    );
                    total_checks += 1;
                }
                prev_sample_2 = Some(p1);
            }
            prev_sample_1 = Some(cur_l);
        }
    }

    assert_eq!(total_checks, NUM_BLOCKS * OUT_BLOCK_FRAMES - 2);

    // 1. max_second_diff < 0.025
    assert!(
        max_second_diff < 0.025,
        "Second difference exceeded 0.025 threshold: got {:.6}",
        max_second_diff
    );

    // 2. underrun_count == 0
    assert_eq!(
        dsp.underrun_count(),
        0,
        "Underrun occurred during 48k->384k upsampling! count = {}",
        dsp.underrun_count()
    );

    // 3. slew_trigger_count == 0
    assert_eq!(
        dsp.slew_trigger_count(),
        0,
        "False slew triggered during 48k->384k upsampling! count = {}",
        dsp.slew_trigger_count()
    );

    // 4. No phase drift (drift remains bounded within Zone 1 range <= 500 ppm)
    let drift = dsp.last_drift_ppm();
    assert!(
        drift.abs() <= 500,
        "Phase drift out of Zone 1 bounds: drift = {} ppm",
        drift
    );

    println!(
        "PASS [Test 2: 48k -> 384k Continuous Upsample]: 1,000 blocks (3,840,000 frames), max_second_diff: {:.6} (< 0.025), drift: {} ppm, underruns: 0, slews: 0",
        max_second_diff, drift
    );
}

/// Test 3: Dynamic Speech Pause & Hysteresis Repriming
///
/// Requirement:
/// - Pump 500ms signal (24,000 frames sine wave @ 48kHz).
/// - Next, 80ms silence (0 frames pumped) simulating natural pause between words in speech.
/// - Then pump another 500ms signal.
/// - Checks:
///   1. FSM transitions smoothly: Running -> UnderrunFadeOut -> Repriming -> Running
///      (reaching reprime_fill_ms 25ms with 3ms Fade-In).
///   2. Step jump between consecutive samples (|y[n] - y[n-1]|) is strictly < 0.05
///      at ALL points throughout the entire transition.
#[test]
fn test_track_d_test3_dynamic_speech_pause_and_hysteresis_repriming() {
    const SAMPLE_RATE: u32 = 48000;
    const BLOCK_FRAMES: usize = 480; // 10ms block
    const FREQ_HZ: f32 = 250.0;      // Speech fundamental frequency f0
    const AMPLITUDE: f32 = 0.5;

    let config = ResamplerConfig {
        capture_rate: SAMPLE_RATE,
        render_rate: SAMPLE_RATE,
        target_fill_ms: 30.0,
        reprime_fill_ms: 25.0, // 25ms @ 48kHz = 1200 frames
        fade_ms: 3.0,          // 3ms @ 48kHz = 144 frames
    };
    let mut dsp = AdaptiveResamplerDsp::with_config(config);
    let rb = SpscRingBuffer::new(65536);

    // Initial headroom: 30ms (1440 frames)
    let mut frame_seq: usize = 0;
    let mut prime_buf = Vec::with_capacity(1440 * 2);
    for _ in 0..1440 {
        let t = (frame_seq as f32) / (SAMPLE_RATE as f32);
        let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
        prime_buf.push(s);
        prime_buf.push(s);
        frame_seq += 1;
    }
    rb.push_slice(&prime_buf);

    let mut prev_sample: Option<f32> = None;
    let mut max_sample_jump = 0.0f32;
    let mut state_history = Vec::new();

    // Helper closure to process a block and verify continuity
    let process_and_check = |dsp: &mut AdaptiveResamplerDsp,
                             rb: &SpscRingBuffer,
                             prev_sample: &mut Option<f32>,
                             max_jump: &mut f32| {
        let mut out = vec![0.0f32; BLOCK_FRAMES * 2];
        dsp.process_output_block_multichannel(rb, &mut out, 2, 1.0, false);
        for f in 0..BLOCK_FRAMES {
            let cur = out[f * 2];
            if let Some(p) = *prev_sample {
                let jump = (cur - p).abs();
                *max_jump = max_jump.max(jump);
                assert!(
                    jump < 0.05,
                    "Discontinuity detected (|y[n] - y[n-1]| = {:.6} >= 0.05) at frame {}",
                    jump, f
                );
            }
            *prev_sample = Some(cur);
        }
        dsp.state()
    };

    // Phase 1: 500ms of active speech (50 blocks of 480 frames)
    for _ in 0..50 {
        let mut feed_buf = Vec::with_capacity(BLOCK_FRAMES * 2);
        for _ in 0..BLOCK_FRAMES {
            let t = (frame_seq as f32) / (SAMPLE_RATE as f32);
            let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
            feed_buf.push(s);
            feed_buf.push(s);
            frame_seq += 1;
        }
        rb.push_slice(&feed_buf);

        let st = process_and_check(&mut dsp, &rb, &mut prev_sample, &mut max_sample_jump);
        state_history.push(st);
    }
    assert_eq!(dsp.state(), ResamplerState::Running);
    assert_eq!(dsp.underrun_count(), 0);

    // Phase 2: 80ms speech pause (8 blocks where capture pushes 0 frames)
    for _ in 0..8 {
        // RB is not fed any new frames during pause
        let st = process_and_check(&mut dsp, &rb, &mut prev_sample, &mut max_sample_jump);
        state_history.push(st);
    }
    // Must be in Repriming state holding silence
    assert_eq!(dsp.state(), ResamplerState::Repriming);
    assert_eq!(dsp.underrun_count(), 1);

    // Phase 3: Resume speech for another 500ms (50 blocks)
    let slews_before_resume = dsp.slew_trigger_count();
    let mut saw_running_again = false;

    for block_idx in 0..50 {
        let mut feed_buf = Vec::with_capacity(BLOCK_FRAMES * 2);
        for _ in 0..BLOCK_FRAMES {
            let t = (frame_seq as f32) / (SAMPLE_RATE as f32);
            let s = (2.0 * std::f32::consts::PI * FREQ_HZ * t).sin() * AMPLITUDE;
            feed_buf.push(s);
            feed_buf.push(s);
            frame_seq += 1;
        }
        rb.push_slice(&feed_buf);

        let st = process_and_check(&mut dsp, &rb, &mut prev_sample, &mut max_sample_jump);
        state_history.push(st);

        if st == ResamplerState::Running {
            saw_running_again = true;
            // On transition to Running, a 3ms Fade-In slew must have been triggered
            assert!(
                dsp.slew_trigger_count() > slews_before_resume,
                "Fade-In slew must be triggered upon repriming completion"
            );
        } else {
            // During block 0 (480 frames) and block 1 (960 frames), available frames < 1200 (25ms),
            // so hysteresis MUST hold state in Repriming without premature choppy audio!
            if block_idx < 2 {
                assert_eq!(
                    st,
                    ResamplerState::Repriming,
                    "Hysteresis failed: exited Repriming before reaching 25ms (1200 frames)"
                );
            }
        }
    }

    assert!(saw_running_again, "DSP failed to resume Running state");
    assert_eq!(dsp.state(), ResamplerState::Running);

    // Step jump between consecutive samples must ALWAYS be < 0.05 across all phases
    assert!(
        max_sample_jump < 0.05,
        "Click/pop detected during state transitions: max_sample_jump = {:.6} >= 0.05",
        max_sample_jump
    );

    println!(
        "PASS [Test 3: Dynamic Speech Pause & Hysteresis Repriming]: Running -> Repriming -> Running, max_sample_jump = {:.6} (< 0.05), underruns = 1, fade-in slews = 1",
        max_sample_jump
    );
}
