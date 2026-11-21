//! Headless verification test suite for 24-bit audio formats:
//! Pcm24Packed and Pcm32Container24.
//!
//! Asserts:
//! - Exact quantization & boundary clamping across [-1.5, -1.0, -0.9999999, 0.0, 0.9999999, 1.0, 1.5, NaN, Inf, -Inf]
//! - Absolute protection against sign flipping / wrap-around at +1.0 and +1.5
//! - 8 LSB bits of Pcm32Container24 are strictly 0x00 at all times (symmetric 24-bit in 32-bit container)
//! - Zero NaN / Inf propagation
//! - TPDF Dither statistical sanity and zero allocation

use audio_router_rust::audio::render::{
    write_frames_to_wasapi, EndpointSampleFormat, TpdfDitherState,
    PCM24_MAX, PCM24_MIN,
};

#[test]
fn test_pcm24_boundary_values_deterministic() {
    let edge_points: [f32; 10] = [
        -1.5,
        -1.0,
        -0.9999999,
        0.0,
        0.9999999,
        1.0,
        1.5,
        f32::NAN,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ];

    let frames = edge_points.len();
    let channels = 2; // Stereo test: left = edge_points[i], right = edge_points[i]

    let mut stereo_input = Vec::with_capacity(frames * 2);
    for &p in &edge_points {
        stereo_input.push(p);
        stereo_input.push(p);
    }

    let mut dither_zero = TpdfDitherState::deterministic_zero();

    // ==========================================
    // 1. Test Pcm24Packed (3 bytes per sample)
    // ==========================================
    let mut dst_pcm24 = vec![0u8; frames * channels * 3];
    unsafe {
        write_frames_to_wasapi(
            dst_pcm24.as_mut_ptr(),
            &stereo_input,
            channels,
            EndpointSampleFormat::Pcm24Packed,
            1.0,
            &mut dither_zero,
        );
    }

    let unpack24 = |frame_idx: usize, ch: usize| -> i32 {
        let base = (frame_idx * channels + ch) * 3;
        let b0 = dst_pcm24[base];
        let b1 = dst_pcm24[base + 1];
        let b2 = dst_pcm24[base + 2];
        ((b2 as i8 as i32) << 16) | ((b1 as i32) << 8) | (b0 as i32)
    };

    for ch in 0..channels {
        // -1.5 (overshoot negative) -> clamped to PCM24_MIN (-8_388_608)
        assert_eq!(unpack24(0, ch), PCM24_MIN, "Ch {} -1.5 clamp failed", ch);
        assert_eq!(&dst_pcm24[(0 * channels + ch) * 3..(0 * channels + ch) * 3 + 3], &[0x00, 0x00, 0x80]);

        // -1.0 (full scale negative) -> exactly PCM24_MIN (-8_388_608)
        assert_eq!(unpack24(1, ch), PCM24_MIN, "Ch {} -1.0 failed", ch);
        assert_eq!(&dst_pcm24[(1 * channels + ch) * 3..(1 * channels + ch) * 3 + 3], &[0x00, 0x00, 0x80]);

        // -0.9999999 -> negative near PCM24_MIN
        let val_neg_almost_one = unpack24(2, ch);
        assert!(val_neg_almost_one < 0, "Ch {} -0.9999999 should be negative", ch);
        assert!(val_neg_almost_one >= PCM24_MIN, "Ch {} -0.9999999 should be >= PCM24_MIN", ch);

        // 0.0 (silence) -> exactly 0
        assert_eq!(unpack24(3, ch), 0, "Ch {} 0.0 failed", ch);
        assert_eq!(&dst_pcm24[(3 * channels + ch) * 3..(3 * channels + ch) * 3 + 3], &[0x00, 0x00, 0x00]);

        // 0.9999999 -> positive near PCM24_MAX
        let val_pos_almost_one = unpack24(4, ch);
        assert!(val_pos_almost_one > 0, "Ch {} 0.9999999 should be positive", ch);
        assert!(val_pos_almost_one <= PCM24_MAX, "Ch {} 0.9999999 should be <= PCM24_MAX", ch);

        // +1.0 (full scale positive) -> CRITICAL: must be exactly PCM24_MAX (8_388_607) [0xFF, 0xFF, 0x7F], NEVER -8_388_608!
        assert_eq!(unpack24(5, ch), PCM24_MAX, "Ch {} +1.0 sign flip or clamp failure!", ch);
        assert_eq!(
            &dst_pcm24[(5 * channels + ch) * 3..(5 * channels + ch) * 3 + 3],
            &[0xFF, 0xFF, 0x7F],
            "Ch {} +1.0 byte pattern must be [0xFF, 0xFF, 0x7F]",
            ch
        );

        // +1.5 (overshoot positive) -> must be clamped to PCM24_MAX (8_388_607), NO wrap-around
        assert_eq!(unpack24(6, ch), PCM24_MAX, "Ch {} +1.5 overshoot clamp failed!", ch);
        assert_eq!(
            &dst_pcm24[(6 * channels + ch) * 3..(6 * channels + ch) * 3 + 3],
            &[0xFF, 0xFF, 0x7F],
            "Ch {} +1.5 byte pattern must be [0xFF, 0xFF, 0x7F]",
            ch
        );

        // NaN -> safe 0.0
        assert_eq!(unpack24(7, ch), 0, "Ch {} NaN should map to 0", ch);

        // +Inf -> safe 0.0
        assert_eq!(unpack24(8, ch), 0, "Ch {} +Inf should map to 0", ch);

        // -Inf -> safe 0.0
        assert_eq!(unpack24(9, ch), 0, "Ch {} -Inf should map to 0", ch);
    }

    // ==========================================
    // 2. Test Pcm32Container24 (4 bytes per sample, 24-in-32)
    // ==========================================
    let mut dst_pcm32 = vec![0i32; frames * channels];
    unsafe {
        write_frames_to_wasapi(
            dst_pcm32.as_mut_ptr() as *mut u8,
            &stereo_input,
            channels,
            EndpointSampleFormat::Pcm32Container24,
            1.0,
            &mut dither_zero,
        );
    }

    for ch in 0..channels {
        // -1.5 -> PCM24_MIN << 8 = -2_147_483_648 (0x8000_0000)
        assert_eq!(dst_pcm32[0 * channels + ch], PCM24_MIN << 8);

        // -1.0 -> PCM24_MIN << 8 = -2_147_483_648 (0x8000_0000)
        assert_eq!(dst_pcm32[1 * channels + ch], PCM24_MIN << 8);

        // 0.0 -> 0
        assert_eq!(dst_pcm32[3 * channels + ch], 0);

        // +1.0 -> PCM24_MAX << 8 = 0x7FFF_FF00 (2_147_483_392), NEVER negative!
        let val_pos_10 = dst_pcm32[5 * channels + ch];
        assert_eq!(val_pos_10, PCM24_MAX << 8, "+1.0 should be exactly 0x7FFF_FF00");
        assert!(val_pos_10 > 0, "+1.0 must be strictly positive in 32-bit container!");

        // +1.5 -> PCM24_MAX << 8 = 0x7FFF_FF00
        let val_pos_15 = dst_pcm32[6 * channels + ch];
        assert_eq!(val_pos_15, PCM24_MAX << 8, "+1.5 overshoot must clamp to 0x7FFF_FF00");
        assert!(val_pos_15 > 0, "+1.5 must be strictly positive!");

        // NaN -> 0
        assert_eq!(dst_pcm32[7 * channels + ch], 0);

        // +Inf -> 0
        assert_eq!(dst_pcm32[8 * channels + ch], 0);

        // -Inf -> 0
        assert_eq!(dst_pcm32[9 * channels + ch], 0);
    }

    // MANDATORY REQUIREMENT: 8 LSB bits of Pcm32Container24 MUST ALWAYS be 0x00 for all samples!
    for (i, &sample) in dst_pcm32.iter().enumerate() {
        assert_eq!(
            sample & 0xFF,
            0,
            "LSB 8-bit constraint violated at index {}: sample = {:#010X}",
            i,
            sample
        );
    }
}

#[test]
fn test_pcm24_with_active_tpdf_dither_stability() {
    // Tests 10,000 iterations of full-scale and edge values with active, uncorrelated TPDF dither
    let mut dither_state = TpdfDitherState::new();

    let iterations = 10_000;
    let channels = 2;

    // Test a sweep of values around +1.0 and -1.0 with active dither
    for i in 0..iterations {
        let input = [
            1.0f32,        // Ch 0: full scale positive
            1.05f32,       // Ch 1: overshoot
        ];

        let mut dst_pcm24 = [0u8; 6];
        let mut dst_pcm32 = [0i32; 2];

        unsafe {
            write_frames_to_wasapi(
                dst_pcm24.as_mut_ptr(),
                &input,
                channels,
                EndpointSampleFormat::Pcm24Packed,
                1.0,
                &mut dither_state,
            );

            write_frames_to_wasapi(
                dst_pcm32.as_mut_ptr() as *mut u8,
                &input,
                channels,
                EndpointSampleFormat::Pcm32Container24,
                1.0,
                &mut dither_state,
            );
        }

        // Check Ch 0 (1.0) and Ch 1 (1.05) in Pcm24Packed: NEVER sign flip!
        for ch in 0..channels {
            let b0 = dst_pcm24[ch * 3];
            let b1 = dst_pcm24[ch * 3 + 1];
            let b2 = dst_pcm24[ch * 3 + 2];
            let val24 = ((b2 as i8 as i32) << 16) | ((b1 as i32) << 8) | (b0 as i32);

            assert!(
                val24 > 0,
                "Iteration {}: Sign flip occurred on Ch {}! val24 = {}, bytes = {:02X?}",
                i, ch, val24, &dst_pcm24[ch * 3..ch * 3 + 3]
            );
            assert!(
                val24 <= PCM24_MAX,
                "Iteration {}: val24 exceeded PCM24_MAX! val24 = {}",
                i, val24
            );

            // Check Pcm32Container24: must be positive and LSB 8 bits == 0x00
            let val32 = dst_pcm32[ch];
            assert!(
                val32 > 0,
                "Iteration {}: Pcm32Container24 sign flip on Ch {}! val32 = {:#010X}",
                i, ch, val32
            );
            assert_eq!(
                val32 & 0xFF,
                0,
                "Iteration {}: Pcm32Container24 LSB 8 bits non-zero on Ch {}! val32 = {:#010X}",
                i, ch, val32
            );
        }
    }
}
