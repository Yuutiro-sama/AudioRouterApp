//! Event-driven WASAPI Multi-Device Render Worker.
//!
//! Each active physical or virtual endpoint (except the default master endpoint) runs
//! a dedicated render thread with MMCSS 'Pro Audio' scheduling, polling its own
//! SPSC Ring Buffer, resampling with 4-point cubic Hermite interpolation, and writing
//! directly to the WASAPI DMA buffer.
//!
//! For the default device, the Zero-Feedback Guard ensures PCM is not re-routed to avoid
//! echo/feedback loops, while master volume/mute is kept in sync via IAudioEndpointVolume.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
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
pub unsafe fn detect_sample_format(pwfx: *const WAVEFORMATEX) -> EndpointSampleFormat {
    if pwfx.is_null() {
        return EndpointSampleFormat::Float32;
    }
    let wfx = &*pwfx;
    let bits = wfx.wBitsPerSample;

    // WAVE_FORMAT_IEEE_FLOAT = 0x0003, WAVE_FORMAT_EXTENSIBLE = 0xFFFE
    if wfx.wFormatTag == 0x0003 {
        return EndpointSampleFormat::Float32;
    }

    if wfx.wFormatTag == 0xFFFE && wfx.cbSize >= 22 {
        let wfxe = &*(pwfx as *const WAVEFORMATEXTENSIBLE);
        let subformat = std::ptr::addr_of!((*wfxe).SubFormat).read_unaligned();
        // KSDATAFORMAT_SUBTYPE_IEEE_FLOAT GUID: 00000003-0000-0010-8000-00AA00389B71
        if subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT || subformat.data1 == 0x00000003 {
            return EndpointSampleFormat::Float32;
        }
    }

    match bits {
        32 => EndpointSampleFormat::Pcm32Container24,
        24 => EndpointSampleFormat::Pcm24Packed,
        16 => EndpointSampleFormat::Pcm16,
        _ => EndpointSampleFormat::Float32,
    }
}

/// Transfers and converts f32 samples [-1.0, 1.0] to native WASAPI DMA buffer format.
pub unsafe fn write_frames_to_wasapi(
    dst_ptr: *mut u8,
    src_interleaved_f32: &[f32],
    channels: usize,
    format: EndpointSampleFormat,
    volume: f32,
) {
    if channels == 0 || src_interleaved_f32.is_empty() || dst_ptr.is_null() {
        return;
    }

    const SCALE_32: f64 = 2147483647.0;
    const SCALE_24: f32 = 8388607.0;
    const SCALE_16: f32 = 32767.0;

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
                    out_slice[i] = (src_interleaved_f32[i] * volume).clamp(-1.0, 1.0);
                }
            } else {
                for i in 0..frames {
                    let (l, r) = if channels == 1 {
                        let m = ((src_interleaved_f32[i * 2] + src_interleaved_f32[i * 2 + 1]) * 0.5 * volume).clamp(-1.0, 1.0);
                        (m, m)
                    } else {
                        (
                            (src_interleaved_f32[i * 2] * volume).clamp(-1.0, 1.0),
                            (src_interleaved_f32[i * 2 + 1] * volume).clamp(-1.0, 1.0),
                        )
                    };
                    out_slice[i * channels] = l;
                    if channels > 1 {
                        out_slice[i * channels + 1] = r;
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
                    let s = (src_interleaved_f32[i] * volume).clamp(-1.0, 1.0) as f64;
                    out_slice[i] = ((s * SCALE_32) as i32) & !0xFF;
                }
            } else {
                for i in 0..frames {
                    let (l, r) = if channels == 1 {
                        let m = ((src_interleaved_f32[i * 2] + src_interleaved_f32[i * 2 + 1]) * 0.5 * volume).clamp(-1.0, 1.0) as f64;
                        (m, m)
                    } else {
                        (
                            (src_interleaved_f32[i * 2] * volume).clamp(-1.0, 1.0) as f64,
                            (src_interleaved_f32[i * 2 + 1] * volume).clamp(-1.0, 1.0) as f64,
                        )
                    };
                    out_slice[i * channels] = ((l * SCALE_32) as i32) & !0xFF;
                    if channels > 1 {
                        out_slice[i * channels + 1] = ((r * SCALE_32) as i32) & !0xFF;
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
                    let val_f = (src_interleaved_f32[i] * volume).clamp(-1.0, 1.0);
                    let val_i32 = (val_f * SCALE_24).round() as i32;
                    let bytes = val_i32.to_le_bytes();
                    let offset = i * 3;
                    out_bytes[offset] = bytes[0];
                    out_bytes[offset + 1] = bytes[1];
                    out_bytes[offset + 2] = bytes[2];
                }
            } else {
                for i in 0..frames {
                    let samples = if channels == 1 {
                        let m = ((src_interleaved_f32[i * 2] + src_interleaved_f32[i * 2 + 1]) * 0.5 * volume).clamp(-1.0, 1.0);
                        [m, m]
                    } else {
                        [
                            (src_interleaved_f32[i * 2] * volume).clamp(-1.0, 1.0),
                            (src_interleaved_f32[i * 2 + 1] * volume).clamp(-1.0, 1.0),
                        ]
                    };
                    for ch in 0..channels {
                        let val_f = if ch < 2 { samples[ch] } else { 0.0 };
                        let val_i32 = (val_f * SCALE_24).round() as i32;
                        let bytes = val_i32.to_le_bytes();
                        let offset = (i * channels + ch) * 3;
                        out_bytes[offset] = bytes[0];
                        out_bytes[offset + 1] = bytes[1];
                        out_bytes[offset + 2] = bytes[2];
                    }
                }
            }
        }
        EndpointSampleFormat::Pcm16 => {
            let out_slice = std::slice::from_raw_parts_mut(dst_ptr as *mut i16, total_dest_samples);
            if is_multichannel_src {
                for i in 0..total_dest_samples {
                    let s = (src_interleaved_f32[i] * volume).clamp(-1.0, 1.0);
                    out_slice[i] = (s * SCALE_16).round() as i16;
                }
            } else {
                for i in 0..frames {
                    let (l, r) = if channels == 1 {
                        let m = ((src_interleaved_f32[i * 2] + src_interleaved_f32[i * 2 + 1]) * 0.5 * volume).clamp(-1.0, 1.0);
                        (m, m)
                    } else {
                        (
                            (src_interleaved_f32[i * 2] * volume).clamp(-1.0, 1.0),
                            (src_interleaved_f32[i * 2 + 1] * volume).clamp(-1.0, 1.0),
                        )
                    };
                    out_slice[i * channels] = (l * SCALE_16).round() as i16;
                    if channels > 1 {
                        out_slice[i * channels + 1] = (r * SCALE_16).round() as i16;
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
    pub clock_drift_ppm: AtomicI32,
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
            clock_drift_ppm: AtomicI32::new(0),
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

    /// Returns current clock drift in parts per million.
    pub fn get_drift_ppm(&self) -> i32 {
        self.clock_drift_ppm.load(Ordering::Acquire)
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
    // WASAPI Quadruple-Buffering: (base_period * 4).max(400_000) (minimum 40ms) for ALL devices
    let buffer_duration = (base_period * 4).max(400_000);

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

    // Unified target_latency_ms = 40ms for ALL audio devices
    let target_latency_ms: u32 = 40;
    let mut dsp = AdaptiveResamplerDsp::new(capture_sample_rate, sample_rate, target_latency_ms);

    // Pre-Roll Buffer Priming:
    // Wait until capture primes ring buffer with at least min_prime_frames (max 300ms)
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

    // Prime the buffer with silence to start stream smoothly
    unsafe {
        let initial_buf = render_client.GetBuffer(buffer_frame_count)?;
        let bytes_per_sample = sample_format.bytes_per_sample();
        std::ptr::write_bytes(initial_buf, 0, (buffer_frame_count as usize) * channels * bytes_per_sample);
        render_client.ReleaseBuffer(buffer_frame_count, 0)?;
        audio_client.Start()?;
    }

    let mut intermediate_f32: Vec<f32> = Vec::with_capacity((buffer_frame_count as usize) * channels);

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

        // Update real-time stats for UI display
        let avail_frames = state.ring_buffer.available_read() / 2;
        let latency_ms = dsp.current_latency_ms(avail_frames);
        state.current_latency_ms.store(latency_ms.to_bits(), Ordering::Release);
        state.clock_drift_ppm.store(dsp.last_drift_ppm(), Ordering::Release);

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
    fn test_write_frames_to_wasapi_all_formats() {
        let input = vec![0.5f32, -0.5f32, 1.2f32, -1.2f32]; // 2 frames stereo with clipping test
        let frames = 2;
        let channels = 2;

        // 1. Float32
        let mut dst_float = vec![0.0f32; frames * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_float.as_mut_ptr() as *mut u8,
                &input,
                channels,
                EndpointSampleFormat::Float32,
                1.0,
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
            );
        }
        let expected_0 = ((0.5 * 2147483647.0) as i32) & !0xFF;
        assert_eq!(dst_pcm32[0], expected_0);
        assert_eq!(dst_pcm32[0] & 0xFF, 0); // 8 LSB bits zeroed for 24-in-32 alignment
        assert_eq!(dst_pcm32[2], 2147483647 & !0xFF); // clamped to +1.0

        // 3. Pcm24Packed (3 bytes per sample)
        let mut dst_pcm24 = vec![0u8; frames * channels * 3];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm24.as_mut_ptr(),
                &input,
                channels,
                EndpointSampleFormat::Pcm24Packed,
                1.0,
            );
        }
        let b0 = dst_pcm24[0];
        let b1 = dst_pcm24[1];
        let b2 = dst_pcm24[2];
        let sample24 = ((b2 as i8 as i32) << 16) | ((b1 as i32) << 8) | (b0 as i32);
        assert!((sample24 - 4194304).abs() <= 1);

        // 4. Pcm16
        let mut dst_pcm16 = vec![0i16; frames * channels];
        unsafe {
            write_frames_to_wasapi(
                dst_pcm16.as_mut_ptr() as *mut u8,
                &input,
                channels,
                EndpointSampleFormat::Pcm16,
                1.0,
            );
        }
        assert_eq!(dst_pcm16[0], 16384); // 0.5 * 32767.0 rounded
        assert_eq!(dst_pcm16[2], 32767); // clamped 1.2 -> 1.0 -> 32767
        assert_eq!(dst_pcm16[3], -32767); // clamped -1.2 -> -1.0 -> -32767
    }
}
