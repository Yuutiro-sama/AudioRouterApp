//! Event-driven WASAPI Multi-Device Render Worker.
//!
//! Each active physical or virtual endpoint (except the default master endpoint) runs
//! a dedicated render thread with MMCSS 'Pro Audio' scheduling, polling its own
//! SPSC Ring Buffer, resampling with 4-point cubic Hermite interpolation, and writing
//! directly to the WASAPI DMA buffer.
//!
//! For the default device, the Zero-Feedback Guard ensures PCM is not re-routed to avoid
//! echo/feedback loops, while master volume/mute is kept in sync via IAudioEndpointVolume.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::core::{GUID, HSTRING};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
    IAudioClient, IAudioRenderClient, IMMDevice,
    IMMDeviceEnumerator, MMDeviceEnumerator, WAVEFORMATEXTENSIBLE,
    WAVEFORMATEX,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryW};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::dsp::AdaptiveResamplerDsp;
use crate::audio::ring_buffer::SpscRingBuffer;

/// Standard GUID for IEEE Float audio subformat: {00000003-0000-0010-8000-00AA00389B71}
pub const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// Standard GUID for PCM audio subformat: {00000001-0000-0010-8000-00AA00389B71}
pub const KSDATAFORMAT_SUBTYPE_PCM: GUID =
    GUID::from_u128(0x00000001_0000_0010_8000_00aa00389b71);

/// Standard 24-bit PCM bounds and scale factor.
pub const PCM24_MIN: i32 = -8_388_608;
pub const PCM24_MAX: i32 = 8_388_607;
pub const PCM24_SCALE: f64 = 8_388_608.0;

/// Standard 16-bit PCM bounds and scale factor.
pub const PCM16_MIN: i32 = -32_768;
pub const PCM16_MAX: i32 = 32_767;
pub const PCM16_SCALE: f64 = 32_767.0;

/// Keep-alive dither floor at -96 dBFS (~1.58489e-5).
/// Injected during silence (|sample| < 1e-6 or 0.0) to prevent aggressive hardware DAC
/// auto-mute / power-gating (e.g. CX31993 at 384kHz) from shutting down analog stages
/// during speech pauses, eliminating DC switching pop when speech resumes.
pub const KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE: f64 = 1.58489e-5;

/// Supported audio sample encoding on WASAPI output endpoints.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointSampleFormat {
    Float32,
    Pcm32Container24, // 32-bit container containing 24-bit (or 32-bit) integer PCM
    Pcm24Packed,      // 24-bit packed: 3 bytes/sample (Little Endian)
    Pcm16,
}

impl EndpointSampleFormat {
    #[inline(always)]
    pub fn bytes_per_sample(&self) -> usize {
        match self {
            EndpointSampleFormat::Float32 => 4,
            EndpointSampleFormat::Pcm32Container24 => 4,
            EndpointSampleFormat::Pcm24Packed => 3,
            EndpointSampleFormat::Pcm16 => 2,
        }
    }
}

/// Reliably detects audio sample format from WASAPI WAVEFORMATEX/WAVEFORMATEXTENSIBLE pointer.
///
/// # Safety
/// The caller must ensure that `pwfx` is either null or points to a valid, readable
/// `WAVEFORMATEX` (or `WAVEFORMATEXTENSIBLE` if cbSize >= 22) structure.
pub unsafe fn detect_sample_format(pwfx: *const WAVEFORMATEX) -> EndpointSampleFormat {
    if pwfx.is_null() {
        return EndpointSampleFormat::Float32;
    }
    let wfx = &*pwfx;
    let bits = wfx.wBitsPerSample;

    // WAVE_FORMAT_IEEE_FLOAT = 0x0003
    if wfx.wFormatTag == 0x0003 {
        return EndpointSampleFormat::Float32;
    }

    // WAVE_FORMAT_EXTENSIBLE = 0xFFFE
    if wfx.wFormatTag == 0xFFFE && wfx.cbSize >= 22 {
        let wfxe = &*(pwfx as *const WAVEFORMATEXTENSIBLE);
        let subformat = std::ptr::addr_of!(wfxe.SubFormat).read_unaligned();
        // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT GUID: 00000003-0000-0010-8000-00AA00389B71
        if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT || subformat.data1 == 0x00000003 {
            return EndpointSampleFormat::Float32;
        }

        let valid_bits = std::ptr::addr_of!(wfxe.Samples.wValidBitsPerSample).read_unaligned();
        if bits == 32 && (valid_bits == 24 || valid_bits == 32 || valid_bits == 0) {
            return EndpointSampleFormat::Pcm32Container24;
        }
        if bits == 24 || valid_bits == 24 {
            return EndpointSampleFormat::Pcm24Packed;
        }
        if bits == 16 || valid_bits == 16 {
            return EndpointSampleFormat::Pcm16;
        }
    }

    match bits {
        32 => EndpointSampleFormat::Pcm32Container24,
        24 => EndpointSampleFormat::Pcm24Packed,
        16 => EndpointSampleFormat::Pcm16,
        _ => EndpointSampleFormat::Float32,
    }
}

/// Zero-allocation, lock-free 32-bit Xorshift PRNG.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct XorShift32 {
    state: u32,
}

impl XorShift32 {
    #[inline(always)]
    pub fn new(seed: u32) -> Self {
        Self {
            state: if seed == 0 { 0x12345678 } else { seed },
        }
    }

    #[inline(always)]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.state = x;
        x
    }

    /// Generates a single TPDF dither value with triangular distribution in [-1.0, 1.0] LSB.
    /// Derived from the difference of two independent uniform random variables in [-0.5, 0.5]:
    /// d = r1 - r2 = (u1 - u2) / 2^32.
    #[inline(always)]
    pub fn next_tpdf_lsb(&mut self) -> f64 {
        let u1 = self.next_u32();
        let u2 = self.next_u32();
        (u1 as f64 - u2 as f64) * (1.0 / 4294967296.0)
    }
}

/// Zero-allocation, lock-free Triangular Probability Density Function (TPDF) Dither generator.
///
/// Maintains independent PRNG state per audio channel (up to 8 channels) to prevent
/// inter-channel noise correlation.
#[derive(Debug, Clone)]
pub struct TpdfDitherState {
    channels: [XorShift32; 8],
    current_ch: usize,
    enabled: bool,
}

impl Default for TpdfDitherState {
    fn default() -> Self {
        Self::new()
    }
}

impl TpdfDitherState {
    /// Creates a new TpdfDitherState with uncorrelated seeds for up to 8 channels.
    pub fn new() -> Self {
        Self {
            channels: [
                XorShift32::new(0x9E3779B9), // Golden ratio seed for Ch 0
                XorShift32::new(0x85EBCA6B), // Ch 1
                XorShift32::new(0xC2B2AE35), // Ch 2
                XorShift32::new(0x27D4EB2F), // Ch 3
                XorShift32::new(0x165667B1), // Ch 4
                XorShift32::new(0x9E3779B1), // Ch 5
                XorShift32::new(0x45D9F3B3), // Ch 6
                XorShift32::new(0x76B197F5), // Ch 7
            ],
            current_ch: 0,
            enabled: true,
        }
    }

    /// Creates a deterministic zero-dither state (returns 0.0 for all dither calls),
    /// useful for bit-exact boundary tests.
    pub fn deterministic_zero() -> Self {
        let mut s = Self::new();
        s.enabled = false;
        s
    }

    /// Gets next TPDF dither value for a specific channel index in range [-1.0, 1.0] LSB.
    #[inline(always)]
    pub fn next_channel_tpdf_lsb(&mut self, channel: usize) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        let ch = channel % self.channels.len();
        self.channels[ch].next_tpdf_lsb()
    }

    /// Gets next TPDF dither value in range [-1.0, 1.0] LSB (rotates channel index).
    #[inline(always)]
    pub fn next_tpdf_lsb(&mut self) -> f64 {
        if !self.enabled {
            return 0.0;
        }
        let ch = self.current_ch;
        self.current_ch = (self.current_ch + 1) % self.channels.len();
        self.channels[ch].next_tpdf_lsb()
    }
}

/// Transfers and converts f32 samples [-1.0, 1.0] to native WASAPI DMA buffer format.
///
/// # Gain Staging Contract
/// Volume and mute processing are unified in the DSP pipeline (`process_output_block_multichannel`).
/// Therefore, callers passing DSP-processed buffers MUST pass `volume = 1.0f32` here to prevent
/// exponential double attenuation (e.g., 0.6 * 0.6 = 0.36).
///
/// # Safety
/// The caller must ensure that `dst_ptr` points to a writable buffer with at least
/// `frames * channels * format.bytes_per_sample()` bytes of allocated memory.
#[allow(clippy::needless_range_loop, clippy::manual_is_multiple_of)]
pub unsafe fn write_frames_to_wasapi(
    dst_ptr: *mut u8,
    src_interleaved_f32: &[f32],
    channels: usize,
    format: EndpointSampleFormat,
    volume: f32,
    dither_state: &mut TpdfDitherState,
) {
    if channels == 0 || src_interleaved_f32.is_empty() || dst_ptr.is_null() {
        return;
    }

    let is_multichannel_src = src_interleaved_f32.len() % channels == 0 && (channels != 2 || src_interleaved_f32.len() != 2);
    let frames = if src_interleaved_f32.len() % channels == 0 {
        src_interleaved_f32.len() / channels
    } else {
        src_interleaved_f32.len() / 2
    };

    let total_dest_samples = frames * channels;

    match format {
        EndpointSampleFormat::Float32 => {
            let out_slice = std::slice::from_raw_parts_mut(dst_ptr as *mut f32, total_dest_samples);
            if is_multichannel_src {
                for i in 0..total_dest_samples {
                    let ch = i % channels;
                    let sample = src_interleaved_f32[i];
                    let mut s = if sample.is_finite() {
                        (sample * volume).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    if s.abs() < 1e-6 {
                        s += (dither_state.next_channel_tpdf_lsb(ch) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE) as f32;
                    }
                    out_slice[i] = s;
                }
            } else {
                for i in 0..frames {
                    let (l, r) = if channels == 1 {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        let m = if sl.is_finite() && sr.is_finite() {
                            ((sl + sr) * 0.5 * volume).clamp(-1.0, 1.0)
                        } else if sl.is_finite() {
                            (sl * volume).clamp(-1.0, 1.0)
                        } else if sr.is_finite() {
                            (sr * volume).clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                        (m, m)
                    } else {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        (
                            if sl.is_finite() { (sl * volume).clamp(-1.0, 1.0) } else { 0.0 },
                            if sr.is_finite() { (sr * volume).clamp(-1.0, 1.0) } else { 0.0 },
                        )
                    };
                    let mut out_l = l;
                    if out_l.abs() < 1e-6 {
                        out_l += (dither_state.next_channel_tpdf_lsb(0) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE) as f32;
                    }
                    out_slice[i * channels] = out_l;
                    if channels > 1 {
                        let mut out_r = r;
                        if out_r.abs() < 1e-6 {
                            out_r += (dither_state.next_channel_tpdf_lsb(1) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE) as f32;
                        }
                        out_slice[i * channels + 1] = out_r;
                    }
                    for ch in 2..channels {
                        out_slice[i * channels + ch] = 0.0;
                    }
                }
            }
        }
        EndpointSampleFormat::Pcm32Container24 => {
            let out_slice = std::slice::from_raw_parts_mut(dst_ptr as *mut i32, total_dest_samples);
            if is_multichannel_src {
                for i in 0..total_dest_samples {
                    let ch = i % channels;
                    let sample = src_interleaved_f32[i];
                    let mut s = if sample.is_finite() {
                        ((sample * volume) as f64).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    if s.abs() < 1e-6 {
                        s += dither_state.next_channel_tpdf_lsb(ch) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                    }
                    let dither = dither_state.next_channel_tpdf_lsb(ch);
                    let q24 = ((s * PCM24_SCALE + dither).round() as i64)
                        .clamp(PCM24_MIN as i64, PCM24_MAX as i64) as i32;
                    out_slice[i] = q24 << 8;
                }
            } else {
                for i in 0..frames {
                    let (l, r) = if channels == 1 {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        let m = if sl.is_finite() && sr.is_finite() {
                            ((sl + sr) * 0.5 * volume).clamp(-1.0, 1.0)
                        } else if sl.is_finite() {
                            (sl * volume).clamp(-1.0, 1.0)
                        } else if sr.is_finite() {
                            (sr * volume).clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                        (m, m)
                    } else {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        (
                            if sl.is_finite() { (sl * volume).clamp(-1.0, 1.0) } else { 0.0 },
                            if sr.is_finite() { (sr * volume).clamp(-1.0, 1.0) } else { 0.0 },
                        )
                    };
                    let mut sl = l as f64;
                    if sl.abs() < 1e-6 {
                        sl += dither_state.next_channel_tpdf_lsb(0) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                    }
                    let dither_l = dither_state.next_channel_tpdf_lsb(0);
                    let q24_l = (((sl) * PCM24_SCALE + dither_l).round() as i64)
                        .clamp(PCM24_MIN as i64, PCM24_MAX as i64) as i32;
                    out_slice[i * channels] = q24_l << 8;

                    if channels > 1 {
                        let mut sr = r as f64;
                        if sr.abs() < 1e-6 {
                            sr += dither_state.next_channel_tpdf_lsb(1) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                        }
                        let dither_r = dither_state.next_channel_tpdf_lsb(1);
                        let q24_r = (((sr) * PCM24_SCALE + dither_r).round() as i64)
                            .clamp(PCM24_MIN as i64, PCM24_MAX as i64) as i32;
                        out_slice[i * channels + 1] = q24_r << 8;
                    }
                    for ch in 2..channels {
                        out_slice[i * channels + ch] = 0;
                    }
                }
            }
        }
        EndpointSampleFormat::Pcm24Packed => {
            let out_bytes = std::slice::from_raw_parts_mut(dst_ptr, total_dest_samples * 3);
            if is_multichannel_src {
                for i in 0..total_dest_samples {
                    let ch = i % channels;
                    let sample = src_interleaved_f32[i];
                    let mut s = if sample.is_finite() {
                        ((sample * volume) as f64).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    if s.abs() < 1e-6 {
                        s += dither_state.next_channel_tpdf_lsb(ch) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                    }
                    let dither = dither_state.next_channel_tpdf_lsb(ch);
                    let q24 = ((s * PCM24_SCALE + dither).round() as i64)
                        .clamp(PCM24_MIN as i64, PCM24_MAX as i64) as i32;
                    let bytes = q24.to_le_bytes();
                    let offset = i * 3;
                    out_bytes[offset] = bytes[0];
                    out_bytes[offset + 1] = bytes[1];
                    out_bytes[offset + 2] = bytes[2];
                }
            } else {
                for i in 0..frames {
                    let samples = if channels == 1 {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        let m = if sl.is_finite() && sr.is_finite() {
                            ((sl + sr) * 0.5 * volume).clamp(-1.0, 1.0)
                        } else if sl.is_finite() {
                            (sl * volume).clamp(-1.0, 1.0)
                        } else if sr.is_finite() {
                            (sr * volume).clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                        [m, m]
                    } else {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        [
                            if sl.is_finite() { (sl * volume).clamp(-1.0, 1.0) } else { 0.0 },
                            if sr.is_finite() { (sr * volume).clamp(-1.0, 1.0) } else { 0.0 },
                        ]
                    };
                    for ch in 0..channels {
                        let offset = (i * channels + ch) * 3;
                        if ch < 2 {
                            let mut s = samples[ch] as f64;
                            if s.abs() < 1e-6 {
                                s += dither_state.next_channel_tpdf_lsb(ch) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                            }
                            let dither = dither_state.next_channel_tpdf_lsb(ch);
                            let q24 = ((s * PCM24_SCALE + dither).round() as i64)
                                .clamp(PCM24_MIN as i64, PCM24_MAX as i64) as i32;
                            let bytes = q24.to_le_bytes();
                            out_bytes[offset] = bytes[0];
                            out_bytes[offset + 1] = bytes[1];
                            out_bytes[offset + 2] = bytes[2];
                        } else {
                            out_bytes[offset] = 0;
                            out_bytes[offset + 1] = 0;
                            out_bytes[offset + 2] = 0;
                        }
                    }
                }
            }
        }
        EndpointSampleFormat::Pcm16 => {
            let out_slice = std::slice::from_raw_parts_mut(dst_ptr as *mut i16, total_dest_samples);
            if is_multichannel_src {
                for i in 0..total_dest_samples {
                    let ch = i % channels;
                    let sample = src_interleaved_f32[i];
                    let mut s = if sample.is_finite() {
                        ((sample * volume) as f64).clamp(-1.0, 1.0)
                    } else {
                        0.0
                    };
                    if s.abs() < 1e-6 {
                        s += dither_state.next_channel_tpdf_lsb(ch) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                    }
                    let dither = dither_state.next_channel_tpdf_lsb(ch);
                    let q16 = ((s * PCM16_SCALE + dither).round() as i64)
                        .clamp(PCM16_MIN as i64, PCM16_MAX as i64) as i16;
                    out_slice[i] = q16;
                }
            } else {
                for i in 0..frames {
                    let (l, r) = if channels == 1 {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        let m = if sl.is_finite() && sr.is_finite() {
                            ((sl + sr) * 0.5 * volume).clamp(-1.0, 1.0)
                        } else if sl.is_finite() {
                            (sl * volume).clamp(-1.0, 1.0)
                        } else if sr.is_finite() {
                            (sr * volume).clamp(-1.0, 1.0)
                        } else {
                            0.0
                        };
                        (m, m)
                    } else {
                        let sl = src_interleaved_f32[i * 2];
                        let sr = src_interleaved_f32[i * 2 + 1];
                        (
                            if sl.is_finite() { (sl * volume).clamp(-1.0, 1.0) } else { 0.0 },
                            if sr.is_finite() { (sr * volume).clamp(-1.0, 1.0) } else { 0.0 },
                        )
                    };
                    let mut sl = l as f64;
                    if sl.abs() < 1e-6 {
                        sl += dither_state.next_channel_tpdf_lsb(0) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                    }
                    let dither_l = dither_state.next_channel_tpdf_lsb(0);
                    let q16_l = (((sl) * PCM16_SCALE + dither_l).round() as i64)
                        .clamp(PCM16_MIN as i64, PCM16_MAX as i64) as i16;
                    out_slice[i * channels] = q16_l;
                    if channels > 1 {
                        let mut sr = r as f64;
                        if sr.abs() < 1e-6 {
                            sr += dither_state.next_channel_tpdf_lsb(1) * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE;
                        }
                        let dither_r = dither_state.next_channel_tpdf_lsb(1);
                        let q16_r = (((sr) * PCM16_SCALE + dither_r).round() as i64)
                            .clamp(PCM16_MIN as i64, PCM16_MAX as i64) as i16;
                        out_slice[i * channels + 1] = q16_r;
                    }
                    for ch in 2..channels {
                        out_slice[i * channels + ch] = 0;
                    }
                }
            }
        }
    }
}

type PfnAvSetMmThreadCharacteristicsW =
    unsafe extern "system" fn(windows::core::PCWSTR, *mut u32) -> HANDLE;
type PfnAvRevertMmThreadCharacteristics =
    unsafe extern "system" fn(HANDLE) -> windows::Win32::Foundation::BOOL;

/// Helper to dynamically activate MMCSS 'Pro Audio' priority without link-time -lavrt dependency.
pub fn set_mmcss_pro_audio() -> Option<(HANDLE, PfnAvRevertMmThreadCharacteristics)> {
    unsafe {
        let avrt = LoadLibraryW(windows::core::w!("avrt.dll")).ok()?;
        let p_set = GetProcAddress(avrt, windows::core::s!("AvSetMmThreadCharacteristicsW"))?;
        let p_rev = GetProcAddress(avrt, windows::core::s!("AvRevertMmThreadCharacteristics"))?;
        let set_fn: PfnAvSetMmThreadCharacteristicsW = std::mem::transmute(p_set);
        let rev_fn: PfnAvRevertMmThreadCharacteristics = std::mem::transmute(p_rev);
        let mut task_idx = 0u32;
        let handle = set_fn(windows::core::w!("Pro Audio"), &mut task_idx);
        if !handle.is_invalid() {
            Some((handle, rev_fn))
        } else {
            None
        }
    }
}

/// Shared runtime state of a single render target endpoint.
pub struct RenderTargetState {
    pub device_id: String,
    pub device_name: String,
    pub is_default: bool,
    pub enabled: AtomicBool,
    pub muted: AtomicBool,
    pub volume_scalar_bits: AtomicU32,
    pub current_latency_ms: AtomicU32,
    pub initial_latency_ms: AtomicU32,
    pub clock_drift_ppm: AtomicI32,
    pub underrun_count: AtomicU64,
    pub slew_trigger_count: AtomicU64,
    pub ring_buffer: Arc<SpscRingBuffer>,
}

impl RenderTargetState {
    /// Creates a new RenderTargetState.
    pub fn new(device_id: String, device_name: String, is_default: bool) -> Self {
        Self {
            device_id,
            device_name,
            is_default,
            enabled: AtomicBool::new(is_default),
            muted: AtomicBool::new(false),
            volume_scalar_bits: AtomicU32::new(1.0f32.to_bits()),
            current_latency_ms: AtomicU32::new(0.0f32.to_bits()),
            initial_latency_ms: AtomicU32::new(0.0f32.to_bits()),
            clock_drift_ppm: AtomicI32::new(0),
            underrun_count: AtomicU64::new(0),
            slew_trigger_count: AtomicU64::new(0),
            ring_buffer: Arc::new(SpscRingBuffer::new(524288)),
        }
    }

    /// Sets the linear output volume scalar [0.0 .. 1.0].
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        self.volume_scalar_bits.store(clamped.to_bits(), Ordering::Release);
    }

    /// Returns the current linear volume scalar [0.0 .. 1.0].
    pub fn get_volume(&self) -> f32 {
        f32::from_bits(self.volume_scalar_bits.load(Ordering::Acquire))
    }

    /// Sets the muted state.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    /// Returns true if muted.
    pub fn is_muted(&self) -> bool {
        self.muted.load(Ordering::Acquire)
    }

    /// Enables or disables output to this endpoint.
    pub fn set_enabled(&self, enabled: bool) {
        self.enabled.store(enabled, Ordering::Release);
    }

    /// Returns true if enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled.load(Ordering::Acquire)
    }

    /// Returns current buffer latency in milliseconds.
    pub fn get_latency_ms(&self) -> f32 {
        f32::from_bits(self.current_latency_ms.load(Ordering::Acquire))
    }

    /// Returns initial buffer latency in milliseconds measured right after initialization.
    pub fn get_initial_latency_ms(&self) -> f32 {
        f32::from_bits(self.initial_latency_ms.load(Ordering::Acquire))
    }

    /// Returns current clock drift in parts per million.
    pub fn get_drift_ppm(&self) -> i32 {
        self.clock_drift_ppm.load(Ordering::Acquire)
    }

    /// Returns total buffer underrun count.
    pub fn get_underrun_count(&self) -> u64 {
        self.underrun_count.load(Ordering::Acquire)
    }

    /// Returns total slew / emergency crossfade events count.
    pub fn get_slew_trigger_count(&self) -> u64 {
        self.slew_trigger_count.load(Ordering::Acquire)
    }
}

/// Spawns a dedicated audio rendering worker thread for the given target endpoint.
pub fn spawn_render_worker(
    state: Arc<RenderTargetState>,
    capture_sample_rate: u32,
    stop_signal: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name(format!("Render-{}", state.device_name))
        .spawn(move || {
            // Enable Flush-to-Zero and Denormals-are-Zero to prevent denormal slow downs in DSP
            #[cfg(target_arch = "x86_64")]
            #[allow(deprecated)]
            unsafe {
                use core::arch::x86_64::{_mm_getcsr, _mm_setcsr};
                let csr = _mm_getcsr();
                _mm_setcsr(csr | 0x8040);
            }

            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

            let mmcss = set_mmcss_pro_audio();

            let res = render_worker_loop(&state, capture_sample_rate, &stop_signal);
            if let Err(e) = res {
                eprintln!("Render worker [{}] stopped with error: {:?}", state.device_name, e);
            }

            if let Some((handle, rev_fn)) = mmcss {
                let _ = unsafe { rev_fn(handle) };
            }

            unsafe { CoUninitialize() };
        })
        .expect("Failed to spawn audio render worker thread")
}

fn render_worker_loop(
    state: &Arc<RenderTargetState>,
    capture_sample_rate: u32,
    stop_signal: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device: IMMDevice =
        unsafe { enumerator.GetDevice(&HSTRING::from(&state.device_id))? };

    // Zero-Feedback Guard:
    // If this endpoint is the default playback device that originates the loopback stream,
    // do NOT render PCM back into it to prevent echo/feedback loops.
    // Instead, control its volume/mute via IAudioEndpointVolume and drain its ring buffer.
    if state.is_default {
        let endpoint_vol: IAudioEndpointVolume =
            unsafe { device.Activate(CLSCTX_ALL, None)? };
        let mut last_vol_bits = u32::MAX;
        let mut last_muted = false;

        while !stop_signal.load(Ordering::Relaxed) && state.enabled.load(Ordering::Relaxed) {
            state.ring_buffer.clear();
            state.current_latency_ms.store(0.0f32.to_bits(), Ordering::Release);
            state.clock_drift_ppm.store(0, Ordering::Release);

            let cur_vol_bits = state.volume_scalar_bits.load(Ordering::Acquire);
            let cur_muted = state.muted.load(Ordering::Acquire);

            if cur_vol_bits != last_vol_bits {
                let vol = f32::from_bits(cur_vol_bits);
                unsafe {
                    let _ = endpoint_vol.SetMasterVolumeLevelScalar(vol, std::ptr::null());
                }
                last_vol_bits = cur_vol_bits;
            }

            if cur_muted != last_muted {
                unsafe {
                    let _ = endpoint_vol.SetMute(cur_muted, std::ptr::null());
                }
                last_muted = cur_muted;
            }

            std::thread::sleep(Duration::from_millis(30));
        }
        return Ok(());
    }

    // Secondary endpoint: Activate IAudioClient
    let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };

    let pwfx = unsafe { audio_client.GetMixFormat()? };
    let sample_rate = unsafe { (*pwfx).nSamplesPerSec };
    let channels = unsafe { (*pwfx).nChannels as usize };
    let sample_format = unsafe { detect_sample_format(pwfx) };

    let mut default_period: i64 = 0;
    let mut min_period: i64 = 0;
    let _ = unsafe { audio_client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period)) };
    let base_period = if default_period > 0 {
        default_period
    } else {
        100_000 // 10ms in 100ns units
    };
    // WASAPI Quintuple-Buffering: (base_period * 5).max(500_000) (minimum 50ms) for ALL devices to absorb DPC spikes
    let buffer_duration = (base_period * 5).max(500_000);

    unsafe {
        audio_client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            buffer_duration,
            0,
            pwfx,
            None,
        )?;
        CoTaskMemFree(Some(pwfx as *const _ as *const _));
    }

    let event: HANDLE = unsafe { CreateEventW(None, false, false, None)? };
    if let Err(e) = unsafe { audio_client.SetEventHandle(event) } {
        let _ = unsafe { CloseHandle(event) };
        return Err(e);
    }

    let render_client: IAudioRenderClient = unsafe { audio_client.GetService()? };
    let buffer_frame_count = unsafe { audio_client.GetBufferSize()? };

    // Unified target_latency_ms = 45ms for ALL audio devices (solid cushion against 5-6ms DPC latency spikes)
    let target_latency_ms: u32 = 45;
    let mut dsp = AdaptiveResamplerDsp::new(capture_sample_rate, sample_rate, target_latency_ms);

    // Pre-Roll Buffer Priming:
    // 1. Drain residual/stale audio in ring buffer before priming
    state.ring_buffer.clear();

    // 2. Wait until capture primes ring buffer with at least min_prime_frames (40ms)
    let prime_start = std::time::Instant::now();
    let min_prime_frames = dsp.target_fill_frames();
    while (state.ring_buffer.available_read() / 2) < min_prime_frames
        && prime_start.elapsed() < Duration::from_millis(300)
        && !stop_signal.load(Ordering::Relaxed)
        && state.enabled.load(Ordering::Relaxed)
    {
        std::thread::sleep(Duration::from_millis(2));
    }

    if stop_signal.load(Ordering::Relaxed) || !state.enabled.load(Ordering::Relaxed) {
        let _ = unsafe { CloseHandle(event) };
        return Ok(());
    }

    // 3. Single-Budget Latency Initialization (Target 40ms):
    // Pre-fill only 1 render period (~10ms) directly from Ring Buffer through DSP,
    // avoiding the 40ms silence pre-load that previously pushed initial latency to ~120ms.
    let period_frames = (((base_period as u64) * (sample_rate as u64) + 5_000_000) / 10_000_000) as u32;
    let initial_frames = period_frames.min(buffer_frame_count).max(1);

    let mut intermediate_f32: Vec<f32> = Vec::with_capacity((buffer_frame_count as usize) * channels);
    let mut dither_state = TpdfDitherState::new();

    let initial_total_samples = (initial_frames as usize) * channels;
    if intermediate_f32.len() < initial_total_samples {
        intermediate_f32.resize(initial_total_samples, 0.0f32);
    }

    let target_vol = state.get_volume();
    let muted = state.is_muted();

    dsp.process_output_block_multichannel(
        &state.ring_buffer,
        &mut intermediate_f32[..initial_total_samples],
        channels,
        target_vol,
        muted,
    );

    unsafe {
        let initial_buf = render_client.GetBuffer(initial_frames)?;
        write_frames_to_wasapi(
            initial_buf,
            &intermediate_f32[..initial_total_samples],
            channels,
            sample_format,
            1.0,
            &mut dither_state,
        );
        render_client.ReleaseBuffer(initial_frames, 0)?;
        audio_client.Start()?;
    }

    let init_avail_frames = state.ring_buffer.available_read() / 2;
    let init_lat = dsp.current_latency_ms(init_avail_frames);
    state.initial_latency_ms.store(init_lat.to_bits(), Ordering::Release);

    while !stop_signal.load(Ordering::Relaxed) && state.enabled.load(Ordering::Relaxed) {
        let wait_res = unsafe { WaitForSingleObject(event, 500) };
        if wait_res != WAIT_OBJECT_0 {
            if stop_signal.load(Ordering::Relaxed) || !state.enabled.load(Ordering::Relaxed) {
                break;
            }
            continue;
        }

        let padding = match unsafe { audio_client.GetCurrentPadding() } {
            Ok(p) => p,
            Err(_) => break,
        };

        let frames_needed = buffer_frame_count.saturating_sub(padding);
        if frames_needed == 0 {
            continue;
        }

        let target_vol = state.get_volume();
        let muted = state.is_muted();

        // Update real-time stats for UI display & telemetry
        let avail_frames = state.ring_buffer.available_read() / 2;
        let latency_ms = dsp.current_latency_ms(avail_frames);
        state.current_latency_ms.store(latency_ms.to_bits(), Ordering::Release);
        state.clock_drift_ppm.store(dsp.last_drift_ppm(), Ordering::Release);
        state.underrun_count.store(dsp.underrun_count(), Ordering::Release);
        state.slew_trigger_count.store(dsp.slew_trigger_count(), Ordering::Release);

        let dest_ptr = match unsafe { render_client.GetBuffer(frames_needed) } {
            Ok(ptr) => ptr,
            Err(_) => break,
        };

        let total_samples = (frames_needed as usize) * channels;
        if intermediate_f32.len() < total_samples {
            intermediate_f32.resize(total_samples, 0.0f32);
        }

        dsp.process_output_block_multichannel(
            &state.ring_buffer,
            &mut intermediate_f32[..total_samples],
            channels,
            target_vol,
            muted,
        );

        unsafe {
            write_frames_to_wasapi(
                dest_ptr,
                &intermediate_f32[..total_samples],
                channels,
                sample_format,
                1.0,
                &mut dither_state,
            );
        }

        let _ = unsafe { render_client.ReleaseBuffer(frames_needed, 0) };
    }

    let _ = unsafe { audio_client.Stop() };
    let _ = unsafe { CloseHandle(event) };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_sample_format_float() {
        let wfx = WAVEFORMATEX {
            wFormatTag: 3, // IEEE_FLOAT
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 2 * 4,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        let fmt = unsafe { detect_sample_format(&wfx) };
        assert_eq!(fmt, EndpointSampleFormat::Float32);

        // WAVEFORMATEXTENSIBLE IEEE_FLOAT
        let wfxe = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE,
                nChannels: 2,
                nSamplesPerSec: 384000,
                nAvgBytesPerSec: 384000 * 2 * 4,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: Default::default(),
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
        };
        let fmt2 = unsafe { detect_sample_format(&wfxe.Format) };
        assert_eq!(fmt2, EndpointSampleFormat::Float32);
    }

    #[test]
    fn test_detect_sample_format_pcm_types() {
        // 32-bit container PCM
        let wfx32 = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 2 * 4,
            nBlockAlign: 8,
            wBitsPerSample: 32,
            cbSize: 0,
        };
        assert_eq!(unsafe { detect_sample_format(&wfx32) }, EndpointSampleFormat::Pcm32Container24);

        // 24-bit packed PCM
        let wfx24 = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 2 * 3,
            nBlockAlign: 6,
            wBitsPerSample: 24,
            cbSize: 0,
        };
        assert_eq!(unsafe { detect_sample_format(&wfx24) }, EndpointSampleFormat::Pcm24Packed);

        // 16-bit PCM
        let wfx16 = WAVEFORMATEX {
            wFormatTag: 1,
            nChannels: 2,
            nSamplesPerSec: 48000,
            nAvgBytesPerSec: 48000 * 2 * 2,
            nBlockAlign: 4,
            wBitsPerSample: 16,
            cbSize: 0,
        };
        assert_eq!(unsafe { detect_sample_format(&wfx16) }, EndpointSampleFormat::Pcm16);
    }

    #[test]
    fn test_detect_sample_format_pcm_extensible() {
        // WAVEFORMATEXTENSIBLE 32-bit container with 24 valid bits
        let mut wfxe32_24 = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE,
                nChannels: 2,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 48000 * 2 * 4,
                nBlockAlign: 8,
                wBitsPerSample: 32,
                cbSize: 22,
            },
            Samples: Default::default(),
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
        };
        wfxe32_24.Samples.wValidBitsPerSample = 24;
        let fmt = unsafe { detect_sample_format(&wfxe32_24.Format) };
        assert_eq!(fmt, EndpointSampleFormat::Pcm32Container24);

        // WAVEFORMATEXTENSIBLE 24-bit packed
        let mut wfxe24 = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE,
                nChannels: 2,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 48000 * 2 * 3,
                nBlockAlign: 6,
                wBitsPerSample: 24,
                cbSize: 22,
            },
            Samples: Default::default(),
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
        };
        wfxe24.Samples.wValidBitsPerSample = 24;
        let fmt24 = unsafe { detect_sample_format(&wfxe24.Format) };
        assert_eq!(fmt24, EndpointSampleFormat::Pcm24Packed);

        // WAVEFORMATEXTENSIBLE 16-bit
        let mut wfxe16 = WAVEFORMATEXTENSIBLE {
            Format: WAVEFORMATEX {
                wFormatTag: 0xFFFE,
                nChannels: 2,
                nSamplesPerSec: 48000,
                nAvgBytesPerSec: 48000 * 2 * 2,
                nBlockAlign: 4,
                wBitsPerSample: 16,
                cbSize: 22,
            },
            Samples: Default::default(),
            dwChannelMask: 3,
            SubFormat: KSDATAFORMAT_SUBTYPE_PCM,
        };
        wfxe16.Samples.wValidBitsPerSample = 16;
        let fmt16 = unsafe { detect_sample_format(&wfxe16.Format) };
        assert_eq!(fmt16, EndpointSampleFormat::Pcm16);
    }

    #[test]
    fn test_tpdf_dither_statistics() {
        let mut dither = TpdfDitherState::new();
        const N: usize = 100_000;
        let mut sum_ch0 = 0.0f64;
        let mut sum_sq_ch0 = 0.0f64;
        let mut sum_ch1 = 0.0f64;
        let mut sum_sq_ch1 = 0.0f64;
        let mut cross_sum = 0.0f64;

        for _ in 0..N {
            let d0 = dither.next_channel_tpdf_lsb(0);
            let d1 = dither.next_channel_tpdf_lsb(1);

            // TPDF support must be strictly bounded in [-1.0, 1.0] LSB
            assert!((-1.0..=1.0).contains(&d0), "d0 out of bounds: {}", d0);
            assert!((-1.0..=1.0).contains(&d1), "d1 out of bounds: {}", d1);

            sum_ch0 += d0;
            sum_sq_ch0 += d0 * d0;
            sum_ch1 += d1;
            sum_sq_ch1 += d1 * d1;
            cross_sum += d0 * d1;
        }

        let mean0 = sum_ch0 / (N as f64);
        let var0 = (sum_sq_ch0 / (N as f64)) - mean0 * mean0;
        let mean1 = sum_ch1 / (N as f64);
        let var1 = (sum_sq_ch1 / (N as f64)) - mean1 * mean1;
        let cov = (cross_sum / (N as f64)) - mean0 * mean1;
        let corr = cov / (var0.sqrt() * var1.sqrt());

        // Zero mean verification (within 0.01 LSB)
        assert!(mean0.abs() < 0.01, "Mean ch0 not near zero: {}", mean0);
        assert!(mean1.abs() < 0.01, "Mean ch1 not near zero: {}", mean1);

        // Theoretical TPDF variance is 1/6 ≈ 0.16667 LSB^2 (allow [0.14, 0.19])
        assert!(var0 > 0.14 && var0 < 0.19, "Variance ch0 out of range: {}", var0);
        assert!(var1 > 0.14 && var1 < 0.19, "Variance ch1 out of range: {}", var1);

        // Inter-channel independence: correlation coefficient must be near 0 (< 0.05)
        assert!(corr.abs() < 0.05, "Channels 0 and 1 are correlated: {}", corr);
    }

    #[test]
    fn test_pcm24_boundary_and_nan_resilience() {
        let test_inputs = [
            -1.5f32,
            -1.0f32,
            -0.9999999f32,
            0.0f32,
            0.9999999f32,
            1.0f32,
            1.5f32,
            f32::NAN,
            f32::INFINITY,
            f32::NEG_INFINITY,
        ];
        let channels = 1;
        let frames = test_inputs.len();

        // 1. Deterministic zero-dither test
        let mut dither_zero = TpdfDitherState::deterministic_zero();

        // Test Pcm24Packed
        let mut dst_pcm24 = vec![0u8; frames * 3];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm24.as_mut_ptr(),
                &test_inputs,
                channels,
                EndpointSampleFormat::Pcm24Packed,
                1.0,
                &mut dither_zero,
            );
        }

        let unpack24 = |idx: usize| -> i32 {
            let b0 = dst_pcm24[idx * 3];
            let b1 = dst_pcm24[idx * 3 + 1];
            let b2 = dst_pcm24[idx * 3 + 2];
            ((b2 as i8 as i32) << 16) | ((b1 as i32) << 8) | (b0 as i32)
        };

        assert_eq!(unpack24(0), PCM24_MIN); // -1.5 -> clamped to -8_388_608
        assert_eq!(unpack24(1), PCM24_MIN); // -1.0 -> -8_388_608
        assert_eq!(unpack24(3), 0);          // 0.0 -> 0
        assert_eq!(unpack24(5), PCM24_MAX); // 1.0 -> 8_388_607 ([0xFF, 0xFF, 0x7F])
        assert_eq!(unpack24(6), PCM24_MAX); // 1.5 -> clamped to 8_388_607 (NO wrap-around!)
        assert_eq!(unpack24(7), 0);          // NaN -> safe 0
        assert_eq!(unpack24(8), 0);          // +Inf -> safe 0
        assert_eq!(unpack24(9), 0);          // -Inf -> safe 0

        // Verify exact byte pattern for +1.0 and +1.5: [0xFF, 0xFF, 0x7F]
        assert_eq!(&dst_pcm24[5 * 3..5 * 3 + 3], &[0xFF, 0xFF, 0x7F]);
        assert_eq!(&dst_pcm24[6 * 3..6 * 3 + 3], &[0xFF, 0xFF, 0x7F]);

        // Test Pcm32Container24
        let mut dst_pcm32 = vec![0i32; frames];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm32.as_mut_ptr() as *mut u8,
                &test_inputs,
                channels,
                EndpointSampleFormat::Pcm32Container24,
                1.0,
                &mut dither_zero,
            );
        }

        assert_eq!(dst_pcm32[0], PCM24_MIN << 8); // -1.5 -> -2_147_483_648 (0x8000_0000)
        assert_eq!(dst_pcm32[1], PCM24_MIN << 8); // -1.0 -> -2_147_483_648
        assert_eq!(dst_pcm32[3], 0);              // 0.0 -> 0
        assert_eq!(dst_pcm32[5], PCM24_MAX << 8); // 1.0 -> 0x7FFF_FF00
        assert_eq!(dst_pcm32[6], PCM24_MAX << 8); // 1.5 -> 0x7FFF_FF00
        assert_eq!(dst_pcm32[7], 0);              // NaN -> safe 0
        assert_eq!(dst_pcm32[8], 0);              // +Inf -> safe 0
        assert_eq!(dst_pcm32[9], 0);              // -Inf -> safe 0

        // 8 LSB bits MUST be 0x00 for all samples in Pcm32Container24
        for (i, &val) in dst_pcm32.iter().enumerate() {
            assert_eq!(val & 0xFF, 0, "LSB 8 bits not zero for index {}: {:#010X}", i, val);
        }

        // 2. Active TPDF dither test: verify clamping even with noise added
        let mut dither_active = TpdfDitherState::new();
        let mut dst_dithered_pcm24 = vec![0u8; frames * 3];
        unsafe {
            write_frames_to_wasapi(
                dst_dithered_pcm24.as_mut_ptr(),
                &test_inputs,
                channels,
                EndpointSampleFormat::Pcm24Packed,
                1.0,
                &mut dither_active,
            );
        }

        for idx in 0..frames {
            let b0 = dst_dithered_pcm24[idx * 3];
            let b1 = dst_dithered_pcm24[idx * 3 + 1];
            let b2 = dst_dithered_pcm24[idx * 3 + 2];
            let sample = ((b2 as i8 as i32) << 16) | ((b1 as i32) << 8) | (b0 as i32);
            assert!(
                (PCM24_MIN..=PCM24_MAX).contains(&sample),
                "Sample {} with dither exceeded bounds: {}",
                idx,
                sample
            );
        }
    }

    #[test]
    fn test_write_frames_to_wasapi_all_formats() {
        let input = vec![0.5f32, -0.5f32, 1.2f32, -1.2f32]; // 2 frames stereo with clipping test
        let frames = 2;
        let channels = 2;
        let mut dither = TpdfDitherState::deterministic_zero();

        // 1. Float32
        let mut dst_float = vec![0.0f32; frames * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_float.as_mut_ptr() as *mut u8,
                &input,
                channels,
                EndpointSampleFormat::Float32,
                1.0,
                &mut dither,
            );
        }
        assert!((dst_float[0] - 0.5).abs() < 1e-6);
        assert!((dst_float[1] - (-0.5)).abs() < 1e-6);
        assert_eq!(dst_float[2], 1.0); // clamped
        assert_eq!(dst_float[3], -1.0); // clamped

        // 2. Pcm32Container24
        let mut dst_pcm32 = vec![0i32; frames * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm32.as_mut_ptr() as *mut u8,
                &input,
                channels,
                EndpointSampleFormat::Pcm32Container24,
                1.0,
                &mut dither,
            );
        }
        assert_eq!(dst_pcm32[0], 4_194_304 << 8);
        assert_eq!(dst_pcm32[1], -4_194_304 << 8);
        assert_eq!(dst_pcm32[0] & 0xFF, 0); // 8 LSB bits zeroed for 24-in-32 alignment
        assert_eq!(dst_pcm32[1] & 0xFF, 0);
        assert_eq!(dst_pcm32[2], PCM24_MAX << 8); // 0x7FFF_FF00 clamped to +1.0
        assert_eq!(dst_pcm32[3], PCM24_MIN << 8); // 0x8000_0000 clamped to -1.0

        // 3. Pcm24Packed (3 bytes per sample)
        let mut dst_pcm24 = vec![0u8; frames * channels * 3];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm24.as_mut_ptr(),
                &input,
                channels,
                EndpointSampleFormat::Pcm24Packed,
                1.0,
                &mut dither,
            );
        }
        let b0 = dst_pcm24[0];
        let b1 = dst_pcm24[1];
        let b2 = dst_pcm24[2];
        let sample24 = ((b2 as i8 as i32) << 16) | ((b1 as i32) << 8) | (b0 as i32);
        assert_eq!(sample24, 4_194_304);

        // Clamped +1.0 full scale for Pcm24Packed: [0xFF, 0xFF, 0x7F]
        let b6 = dst_pcm24[6];
        let b7 = dst_pcm24[7];
        let b8 = dst_pcm24[8];
        assert_eq!([b6, b7, b8], [0xFF, 0xFF, 0x7F]);

        // Clamped -1.0 full scale for Pcm24Packed: [0x00, 0x00, 0x80]
        let b9 = dst_pcm24[9];
        let b10 = dst_pcm24[10];
        let b11 = dst_pcm24[11];
        assert_eq!([b9, b10, b11], [0x00, 0x00, 0x80]);

        // 4. Pcm16
        let mut dst_pcm16 = vec![0i16; frames * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm16.as_mut_ptr() as *mut u8,
                &input,
                channels,
                EndpointSampleFormat::Pcm16,
                1.0,
                &mut dither,
            );
        }
        assert_eq!(dst_pcm16[0], 16384); // 0.5 * 32767.0 rounded
        assert_eq!(dst_pcm16[2], 32767); // clamped 1.2 -> 1.0 -> 32767
        assert_eq!(dst_pcm16[3], -32767); // clamped -1.2 -> -1.0 -> -32767
    }

    #[test]
    fn test_keep_alive_dither_floor_injection() {
        let input_silence = vec![0.0f32; 100 * 2]; // 100 frames stereo silence
        let channels = 2;

        // 1. Deterministic zero dither -> strict 0.0 across all formats
        let mut dither_zero = TpdfDitherState::deterministic_zero();
        let mut dst_float_zero = vec![0.0f32; 100 * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_float_zero.as_mut_ptr() as *mut u8,
                &input_silence,
                channels,
                EndpointSampleFormat::Float32,
                1.0,
                &mut dither_zero,
            );
        }
        for &s in &dst_float_zero {
            assert_eq!(s, 0.0);
        }

        // 2. Active dither -> Keep-Alive Dither Floor injected at -96 dBFS (~1.58e-5)
        let mut dither_active = TpdfDitherState::new();
        let mut dst_float_active = vec![0.0f32; 100 * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_float_active.as_mut_ptr() as *mut u8,
                &input_silence,
                channels,
                EndpointSampleFormat::Float32,
                1.0,
                &mut dither_active,
            );
        }

        let mut has_non_zero = false;
        let mut max_abs = 0.0f32;
        for &s in &dst_float_active {
            if s.abs() > 0.0 {
                has_non_zero = true;
            }
            max_abs = max_abs.max(s.abs());
            // Must be bounded within +/- 2.0 * KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE
            assert!(
                s.abs() <= (KEEP_ALIVE_DITHER_FLOOR_AMPLITUDE as f32 * 2.0),
                "Keep-alive floor exceeded limit: {}",
                s
            );
        }
        assert!(has_non_zero, "Keep-alive dither was not injected for silence input!");
        assert!(max_abs > 0.0 && max_abs < 3.2e-5);
    }
}
