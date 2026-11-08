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
            ring_buffer: Arc::new(SpscRingBuffer::default()),
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
    let bits_per_sample = unsafe { (*pwfx).wBitsPerSample };

    let is_float = unsafe {
        if (*pwfx).wFormatTag == 3 {
            true
        } else if (*pwfx).wFormatTag == 0xFFFE {
            let ext = pwfx as *const WAVEFORMATEXTENSIBLE;
            let subformat = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
            subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        } else {
            false
        }
    };

    let mut default_period: i64 = 0;
    unsafe {
        let _ = audio_client.GetDevicePeriod(Some(&mut default_period), None);
    }
    let buffer_duration = if default_period > 0 {
        default_period
    } else {
        100_000 // 10ms in 100ns units
    };

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

    // Determine target latency: Bluetooth endpoints benefit from ~35ms, wired ~15ms
    let is_bluetooth = state.device_name.to_lowercase().contains("bluetooth")
        || state.device_name.to_lowercase().contains("a2dp")
        || state.device_name.to_lowercase().contains("hands-free")
        || state.device_name.to_lowercase().contains("wireless");
    let target_latency_ms = if is_bluetooth { 35 } else { 15 };

    let mut dsp = AdaptiveResamplerDsp::new(capture_sample_rate, sample_rate, target_latency_ms);

    // Prime the buffer with silence to start stream smoothly
    unsafe {
        let initial_buf = render_client.GetBuffer(buffer_frame_count)?;
        let bytes_per_sample = (bits_per_sample as usize) / 8;
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

        if is_float && bits_per_sample == 32 {
            unsafe {
                std::ptr::copy_nonoverlapping(
                    intermediate_f32.as_ptr() as *const u8,
                    dest_ptr,
                    total_samples * std::mem::size_of::<f32>(),
                );
            }
        } else if bits_per_sample == 16 {
            let dest_slice =
                unsafe { std::slice::from_raw_parts_mut(dest_ptr as *mut i16, total_samples) };
            for (i, &s) in intermediate_f32[..total_samples].iter().enumerate() {
                dest_slice[i] = (s * 32767.0).clamp(-32768.0, 32767.0) as i16;
            }
        } else {
            let bytes_per_sample = (bits_per_sample as usize) / 8;
            unsafe {
                std::ptr::write_bytes(dest_ptr, 0, total_samples * bytes_per_sample);
            }
        }

        let _ = unsafe { render_client.ReleaseBuffer(frames_needed, 0) };
    }

    let _ = unsafe { audio_client.Stop() };
    let _ = unsafe { CloseHandle(event) };

    Ok(())
}
