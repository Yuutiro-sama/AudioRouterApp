//! High-Performance Real-Time Dual-Output Audio Bridge.
//!
//! Captures loopback audio from a designated source render endpoint (Primary Device A)
//! and resamples & renders it into a designated target render endpoint (Secondary Device B)
//! with sub-1ms internal latency, 4-point Cubic Hermite interpolation, and PI clock drift compensation.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::dsp::{AdaptiveResamplerDsp, ResamplerState};
use crate::audio::render::{
    detect_sample_format, set_mmcss_pro_audio, write_frames_to_wasapi, EndpointSampleFormat,
    TpdfDitherState,
};
use crate::audio::ring_buffer::SpscRingBuffer;

/// Active bridge connecting a source output device to a secondary target device.
pub struct DualOutputBridge {
    pub target_pid: u32,
    pub source_device_id: String,
    pub target_device_id: String,
    pub volume_scalar_bits: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub latency_ms: Arc<AtomicU32>,
    pub initial_latency_ms: Arc<AtomicU32>,
    pub drift_ppm: Arc<AtomicI32>,
    pub underrun_count: Arc<AtomicU64>,
    pub slew_trigger_count: Arc<AtomicU64>,
    pub dsp_state: Arc<AtomicU8>,
    stop_signal: Arc<AtomicBool>,
    capture_handle: Option<std::thread::JoinHandle<()>>,
    render_handle: Option<std::thread::JoinHandle<()>>,
}

impl DualOutputBridge {
    /// Spawns a new real-time bridge from `target_pid` / `source_device_id` to `target_device_id`.
    /// If `target_pid > 0`, captures process-specific loopback audio via Windows Application Loopback API.
    /// If `target_pid == 0`, captures loopback from `source_device_id` (or default endpoint if empty).
    pub fn start(
        target_pid: u32,
        source_device_id: String,
        target_device_id: String,
    ) -> Result<Self, windows::core::Error> {
        if !source_device_id.is_empty() && source_device_id == target_device_id {
            return Err(windows::core::Error::new(
                windows::Win32::Foundation::E_INVALIDARG,
                "Source and target devices cannot be the same",
            ));
        }
        let ring_buffer = Arc::new(SpscRingBuffer::new(524288));
        let stop_signal = Arc::new(AtomicBool::new(false));
        let volume_scalar_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let muted = Arc::new(AtomicBool::new(false));
        let latency_ms = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let initial_latency_ms = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let drift_ppm = Arc::new(AtomicI32::new(0));
        let underrun_count = Arc::new(AtomicU64::new(0));
        let slew_trigger_count = Arc::new(AtomicU64::new(0));
        let dsp_state = Arc::new(AtomicU8::new(ResamplerState::Priming as u8));

        let source_rate = Arc::new(AtomicU32::new(0));

        // 1. Spawn Capture Thread
        let rb_cap = ring_buffer.clone();
        let stop_cap = stop_signal.clone();
        let src_id = source_device_id.clone();
        let src_rate_cap = source_rate.clone();

        let thread_name = if target_pid > 0 {
            format!("BridgeCap-PID{}", target_pid)
        } else {
            format!("BridgeCap-{}", &source_device_id[..source_device_id.len().min(8)])
        };

        let capture_handle = std::thread::Builder::new()
            .name(thread_name)
            .spawn(move || {
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let mmcss = set_mmcss_pro_audio();

                let res = capture_worker_loop(target_pid, &src_id, &rb_cap, &src_rate_cap, &stop_cap);
                if let Err(e) = res {
                    eprintln!("Capture worker [PID: {}, dev: {}] error: {:?}", target_pid, src_id, e);
                }

                if let Some((handle, rev_fn)) = mmcss {
                    let _ = unsafe { rev_fn(handle) };
                }
                unsafe { CoUninitialize() };
            })
            .map_err(|_| windows::core::Error::from_win32())?;

        // 2. Spawn Render Thread
        let rb_ren = ring_buffer.clone();
        let stop_ren = stop_signal.clone();
        let tgt_id = target_device_id.clone();
        let vol_bits = volume_scalar_bits.clone();
        let mute_flag = muted.clone();
        let lat_atomic = latency_ms.clone();
        let init_lat_atomic = initial_latency_ms.clone();
        let drift_atomic = drift_ppm.clone();
        let underrun_atomic = underrun_count.clone();
        let slew_atomic = slew_trigger_count.clone();
        let state_atomic = dsp_state.clone();
        let src_rate_ren = source_rate.clone();

        let render_handle = std::thread::Builder::new()
            .name(format!("BridgeRen-{}", &target_device_id[..target_device_id.len().min(8)]))
            .spawn(move || {
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let mmcss = set_mmcss_pro_audio();

                let res = render_worker_loop(
                    &tgt_id,
                    &rb_ren,
                    &src_rate_ren,
                    &vol_bits,
                    &mute_flag,
                    &lat_atomic,
                    &init_lat_atomic,
                    &drift_atomic,
                    &underrun_atomic,
                    &slew_atomic,
                    &state_atomic,
                    &stop_ren,
                );
                if let Err(e) = res {
                    eprintln!("Render worker [{}] error: {:?}", tgt_id, e);
                }

                if let Some((handle, rev_fn)) = mmcss {
                    let _ = unsafe { rev_fn(handle) };
                }
                unsafe { CoUninitialize() };
            })
            .map_err(|_| windows::core::Error::from_win32())?;

        Ok(Self {
            target_pid,
            source_device_id,
            target_device_id,
            volume_scalar_bits,
            muted,
            latency_ms,
            initial_latency_ms,
            drift_ppm,
            underrun_count,
            slew_trigger_count,
            dsp_state,
            stop_signal,
            capture_handle: Some(capture_handle),
            render_handle: Some(render_handle),
        })
    }

    /// Returns initial buffer latency in milliseconds measured right after initialization.
    pub fn get_initial_latency_ms(&self) -> f32 {
        f32::from_bits(self.initial_latency_ms.load(Ordering::Acquire))
    }

    /// Returns total buffer underrun count.
    pub fn get_underrun_count(&self) -> u64 {
        self.underrun_count.load(Ordering::Acquire)
    }

    /// Returns total slew / emergency crossfade events count.
    pub fn get_slew_trigger_count(&self) -> u64 {
        self.slew_trigger_count.load(Ordering::Acquire)
    }

    /// Returns the current FSM state of the adaptive resampler DSP engine.
    pub fn get_dsp_state(&self) -> ResamplerState {
        ResamplerState::from_u8(self.dsp_state.load(Ordering::Acquire))
    }

    /// Sets the volume scalar [0.0 .. 1.0] for this bridge.
    pub fn set_volume(&self, volume: f32) {
        let clamped = volume.clamp(0.0, 1.0);
        self.volume_scalar_bits.store(clamped.to_bits(), Ordering::Release);
    }

    /// Sets the mute status for this bridge.
    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Ordering::Release);
    }

    /// Signals threads to stop and joins them.
    pub fn stop(&mut self) {
        self.stop_signal.store(true, Ordering::Release);
        if let Some(h) = self.capture_handle.take() {
            let _ = h.join();
        }
        if let Some(h) = self.render_handle.take() {
            let _ = h.join();
        }
    }
}

impl Drop for DualOutputBridge {
    fn drop(&mut self) {
        self.stop();
    }
}

fn capture_worker_loop(
    target_pid: u32,
    source_id: &str,
    rb: &Arc<SpscRingBuffer>,
    source_rate: &Arc<AtomicU32>,
    stop_signal: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let (audio_client, is_process_loopback) = if target_pid > 0 {
        match crate::audio::capture::activate_process_loopback_client(target_pid) {
            Ok(client) => (client, true),
            Err(e) => {
                eprintln!(
                    "Process loopback activation failed for PID {}: {:?}. Falling back to endpoint capture on [{}]",
                    target_pid, e, source_id
                );
                (activate_endpoint_audio_client(source_id)?, false)
            }
        }
    } else {
        (activate_endpoint_audio_client(source_id)?, false)
    };

    let (sample_rate, channels, sample_format, init_res) = if is_process_loopback {
        // VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK does not implement GetMixFormat().
        // Standard 48kHz 32-bit float stereo is the universally supported format for process loopback.
        let sample_rate = 48000u32;
        let channels = 2usize;
        let sample_format = EndpointSampleFormat::Float32;

        let wfx_ext = crate::audio::capture::create_waveformat_float_stereo(sample_rate);
        let pwfx = &wfx_ext as *const _ as *const windows::Win32::Media::Audio::WAVEFORMATEX;
        let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let res = unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                100_000, // 10ms
                0,
                pwfx,
                None,
            )
        };
        (sample_rate, channels, sample_format, res)
    } else {
        let pwfx = unsafe { audio_client.GetMixFormat()? };
        let sample_rate = unsafe { (*pwfx).nSamplesPerSec };
        let channels = unsafe { (*pwfx).nChannels as usize };
        let sample_format = unsafe { detect_sample_format(pwfx) };

        let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
        let res = unsafe {
            audio_client.Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                stream_flags,
                100_000, // 10ms
                0,
                pwfx,
                None,
            )
        };
        unsafe { CoTaskMemFree(Some(pwfx as *const _ as *const _)) };
        (sample_rate, channels, sample_format, res)
    };

    source_rate.store(sample_rate, Ordering::Release);

    let (use_event, event) = match init_res {
        Ok(_) => {
            let evt = unsafe { CreateEventW(None, false, false, None)? };
            if unsafe { audio_client.SetEventHandle(evt) }.is_ok() {
                (true, evt)
            } else {
                let _ = unsafe { CloseHandle(evt) };
                (false, HANDLE::default())
            }
        }
        Err(_) => {
            if is_process_loopback {
                let wfx_ext = crate::audio::capture::create_waveformat_float_stereo(sample_rate);
                let pwfx = &wfx_ext as *const _ as *const windows::Win32::Media::Audio::WAVEFORMATEX;
                unsafe {
                    audio_client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        100_000,
                        0,
                        pwfx,
                        None,
                    )?;
                }
            } else {
                let pwfx2 = unsafe { audio_client.GetMixFormat()? };
                unsafe {
                    audio_client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        AUDCLNT_STREAMFLAGS_LOOPBACK,
                        100_000,
                        0,
                        pwfx2,
                        None,
                    )?;
                    CoTaskMemFree(Some(pwfx2 as *const _ as *const _));
                }
            }
            (false, HANDLE::default())
        }
    };

    let capture_client: IAudioCaptureClient = unsafe { audio_client.GetService()? };
    unsafe { audio_client.Start()? };

    let mut stereo_staging = vec![0.0f32; 16384];

    while !stop_signal.load(Ordering::Relaxed) {
        if use_event {
            let wait_res = unsafe { WaitForSingleObject(event, 100) };
            if wait_res != WAIT_OBJECT_0 && stop_signal.load(Ordering::Relaxed) {
                break;
            }
        } else {
            std::thread::sleep(Duration::from_millis(5));
        }

        loop {
            let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames_read: u32 = 0;
            let mut flags: u32 = 0;

            let hr = unsafe {
                capture_client.GetBuffer(
                    &mut buffer_ptr,
                    &mut num_frames_read,
                    &mut flags,
                    None,
                    None,
                )
            };

            if hr.is_err() || num_frames_read == 0 {
                break;
            }

            let is_silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            if is_silent {
                if num_frames_read > 0 {
                    rb.push_silence(num_frames_read as usize);
                }
                let _ = unsafe { capture_client.ReleaseBuffer(num_frames_read) };
                continue;
            }

            let total_stereo_samples = (num_frames_read as usize) * 2;
            if stereo_staging.len() < total_stereo_samples {
                stereo_staging.resize(total_stereo_samples, 0.0f32);
            }

            match sample_format {
                EndpointSampleFormat::Float32 => {
                    let src_slice = unsafe {
                        std::slice::from_raw_parts(
                            buffer_ptr as *const f32,
                            (num_frames_read as usize) * channels,
                        )
                    };
                    for frame in 0..(num_frames_read as usize) {
                        let base_src = frame * channels;
                        let (l, r) = match channels {
                            1 => (src_slice[base_src], src_slice[base_src]),
                            _ => (src_slice[base_src], src_slice[base_src + 1]),
                        };
                        stereo_staging[frame * 2] = l;
                        stereo_staging[frame * 2 + 1] = r;
                    }
                }
                EndpointSampleFormat::Pcm32Container24 => {
                    let src_slice = unsafe {
                        std::slice::from_raw_parts(
                            buffer_ptr as *const i32,
                            (num_frames_read as usize) * channels,
                        )
                    };
                    for frame in 0..(num_frames_read as usize) {
                        let base_src = frame * channels;
                        let (l, r) = match channels {
                            1 => (
                                src_slice[base_src] as f32 / 2147483648.0,
                                src_slice[base_src] as f32 / 2147483648.0,
                            ),
                            _ => (
                                src_slice[base_src] as f32 / 2147483648.0,
                                src_slice[base_src + 1] as f32 / 2147483648.0,
                            ),
                        };
                        stereo_staging[frame * 2] = l;
                        stereo_staging[frame * 2 + 1] = r;
                    }
                }
                EndpointSampleFormat::Pcm24Packed => {
                    let src_bytes = unsafe {
                        std::slice::from_raw_parts(
                            buffer_ptr,
                            (num_frames_read as usize) * channels * 3,
                        )
                    };
                    for frame in 0..(num_frames_read as usize) {
                        let base_src = frame * channels * 3;
                        let get_sample = |ch_offset: usize| -> f32 {
                            let b0 = src_bytes[ch_offset] as i32;
                            let b1 = src_bytes[ch_offset + 1] as i32;
                            let b2 = src_bytes[ch_offset + 2] as i8 as i32;
                            let val = (b2 << 16) | (b1 << 8) | b0;
                            val as f32 / 8388608.0
                        };
                        let (l, r) = match channels {
                            1 => {
                                let s = get_sample(base_src);
                                (s, s)
                            }
                            _ => (get_sample(base_src), get_sample(base_src + 3)),
                        };
                        stereo_staging[frame * 2] = l;
                        stereo_staging[frame * 2 + 1] = r;
                    }
                }
                EndpointSampleFormat::Pcm16 => {
                    let src_slice = unsafe {
                        std::slice::from_raw_parts(
                            buffer_ptr as *const i16,
                            (num_frames_read as usize) * channels,
                        )
                    };
                    for frame in 0..(num_frames_read as usize) {
                        let base_src = frame * channels;
                        let (l, r) = match channels {
                            1 => (
                                src_slice[base_src] as f32 / 32768.0,
                                src_slice[base_src] as f32 / 32768.0,
                            ),
                            _ => (
                                src_slice[base_src] as f32 / 32768.0,
                                src_slice[base_src + 1] as f32 / 32768.0,
                            ),
                        };
                        stereo_staging[frame * 2] = l;
                        stereo_staging[frame * 2 + 1] = r;
                    }
                }
            }

            rb.push_slice(&stereo_staging[..total_stereo_samples]);
            let _ = unsafe { capture_client.ReleaseBuffer(num_frames_read) };
        }
    }

    let _ = unsafe { audio_client.Stop() };
    if use_event {
        let _ = unsafe { CloseHandle(event) };
    }

    Ok(())
}

fn activate_endpoint_audio_client(source_id: &str) -> windows::core::Result<IAudioClient> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let device: IMMDevice = if source_id.is_empty() {
        unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? }
    } else {
        unsafe { enumerator.GetDevice(&HSTRING::from(source_id))? }
    };

    unsafe { device.Activate(CLSCTX_ALL, None) }
}

#[allow(clippy::too_many_arguments)]
fn render_worker_loop(
    target_id: &str,
    rb: &Arc<SpscRingBuffer>,
    source_rate: &Arc<AtomicU32>,
    volume_bits: &Arc<AtomicU32>,
    muted_flag: &Arc<AtomicBool>,
    lat_atomic: &Arc<AtomicU32>,
    init_lat_atomic: &Arc<AtomicU32>,
    drift_atomic: &Arc<AtomicI32>,
    underrun_atomic: &Arc<AtomicU64>,
    slew_atomic: &Arc<AtomicU64>,
    state_atomic: &Arc<AtomicU8>,
    stop_signal: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device: IMMDevice = unsafe { enumerator.GetDevice(&HSTRING::from(target_id))? };

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
        100_000 // 10ms
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

    // Wait until capture reports source sample rate (max 500ms)
    let mut in_rate = source_rate.load(Ordering::Acquire);
    let start_wait = std::time::Instant::now();
    while in_rate == 0 && start_wait.elapsed() < Duration::from_millis(500) {
        std::thread::sleep(Duration::from_millis(10));
        in_rate = source_rate.load(Ordering::Acquire);
    }
    if in_rate == 0 {
        in_rate = 48000;
    }

    // Unified target_latency_ms = 45ms for ALL audio devices (solid cushion against 5-6ms DPC latency spikes)
    let target_latency_ms: u32 = 45;
    let mut dsp = AdaptiveResamplerDsp::new(in_rate, sample_rate, target_latency_ms);

    // Pre-Roll Buffer Priming:
    // 1. Drain residual/stale audio in ring buffer before priming
    rb.clear();

    // 2. Wait until capture primes ring buffer with at least min_prime_frames (40ms)
    let prime_start = std::time::Instant::now();
    let min_prime_frames = dsp.target_fill_frames();
    while (rb.available_read() / 2) < min_prime_frames
        && prime_start.elapsed() < Duration::from_millis(300)
        && !stop_signal.load(Ordering::Relaxed)
    {
        std::thread::sleep(Duration::from_millis(2));
    }

    if stop_signal.load(Ordering::Relaxed) {
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

    let target_vol = f32::from_bits(volume_bits.load(Ordering::Acquire));
    let muted = muted_flag.load(Ordering::Acquire);

    dsp.process_output_block_multichannel(
        rb,
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

    let init_avail = rb.available_read() / 2;
    let init_lat = dsp.current_latency_ms(init_avail);
    init_lat_atomic.store(init_lat.to_bits(), Ordering::Release);

    while !stop_signal.load(Ordering::Relaxed) {
        let wait_res = unsafe { WaitForSingleObject(event, 500) };
        if wait_res != WAIT_OBJECT_0 {
            if stop_signal.load(Ordering::Relaxed) {
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

        // Dynamically update input rate if it changed
        let cur_in_rate = source_rate.load(Ordering::Relaxed);
        if cur_in_rate > 0 && cur_in_rate != dsp.in_rate() {
            dsp = AdaptiveResamplerDsp::new(cur_in_rate, sample_rate, target_latency_ms);
        }

        let target_vol = f32::from_bits(volume_bits.load(Ordering::Acquire));
        let muted = muted_flag.load(Ordering::Acquire);

        // Update real-time stats & telemetry
        let avail_frames = rb.available_read() / 2;
        let latency = dsp.current_latency_ms(avail_frames);
        lat_atomic.store(latency.to_bits(), Ordering::Release);
        drift_atomic.store(dsp.last_drift_ppm(), Ordering::Release);
        underrun_atomic.store(dsp.underrun_count(), Ordering::Release);
        slew_atomic.store(dsp.slew_trigger_count(), Ordering::Release);

        let dest_ptr = match unsafe { render_client.GetBuffer(frames_needed) } {
            Ok(ptr) => ptr,
            Err(_) => break,
        };

        let total_samples = (frames_needed as usize) * channels;
        if intermediate_f32.len() < total_samples {
            intermediate_f32.resize(total_samples, 0.0f32);
        }

        dsp.process_output_block_multichannel(
            rb,
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
        state_atomic.store(dsp.state() as u8, Ordering::Release);
    }

    let _ = unsafe { audio_client.Stop() };
    let _ = unsafe { CloseHandle(event) };

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::enumerator::enumerate_render_devices;
    use crate::audio::render::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;

    #[test]
    fn test_print_all_device_formats() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if let Ok(devices) = enumerate_render_devices() {
            println!("\n=== AUDIO RENDER ENDPOINTS FORMATS ===");
            let enumerator: IMMDeviceEnumerator =
                unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).unwrap() };
            for d in devices {
                if let Ok(device) = unsafe { enumerator.GetDevice(&HSTRING::from(&d.id)) } {
                    if let Ok(client) = unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) } {
                        if let Ok(pwfx) = unsafe { client.GetMixFormat() } {
                            let tag = unsafe { (*pwfx).wFormatTag };
                            let rate = unsafe { (*pwfx).nSamplesPerSec };
                            let ch = unsafe { (*pwfx).nChannels };
                            let bits = unsafe { (*pwfx).wBitsPerSample };
                            let (is_float, valid_bits, subformat_guid) = unsafe {
                                if tag == 3 {
                                    (true, bits, windows::core::GUID::zeroed())
                                } else if tag == 0xFFFE {
                                    let ext = pwfx as *const WAVEFORMATEXTENSIBLE;
                                    let subformat = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
                                    let valid = std::ptr::addr_of!((*ext).Samples.wValidBitsPerSample).read_unaligned();
                                    (subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, valid, subformat)
                                } else {
                                    (false, bits, windows::core::GUID::zeroed())
                                }
                            };
                            println!(
                                "Device: [{}] -> Tag: 0x{:X}, Rate: {} Hz, Ch: {}, Bits: {}, ValidBits: {}, Float: {}, SubFormat: {:?}",
                                d.name, tag, rate, ch, bits, valid_bits, is_float, subformat_guid
                            );
                            unsafe { CoTaskMemFree(Some(pwfx as *const _ as *const _)) };
                        }
                    }
                }
            }
            println!("======================================");
        }
        unsafe { CoUninitialize() };
    }

    #[test]
    fn test_live_dual_bridge_start_and_run() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let devices = enumerate_render_devices().unwrap();
        // Find Realtek and CX31993 specifically
        let realtek = devices.iter().find(|d| d.name.contains("Realtek"));
        let cx = devices.iter().find(|d| d.name.contains("CX31993"));

        if let (Some(r), Some(c)) = (realtek, cx) {
            println!("Testing 48kHz -> 384kHz Bridge: [{}] -> [{}]", r.name, c.name);
            let mut bridge = DualOutputBridge::start(0, r.id.clone(), c.id.clone()).expect("Failed to start bridge");
            std::thread::sleep(Duration::from_millis(600));
            let lat = f32::from_bits(bridge.latency_ms.load(Ordering::Acquire));
            let drift = bridge.drift_ppm.load(Ordering::Acquire);
            println!("Bridge 48k->384k running: latency = {:.2} ms, drift = {} ppm", lat, drift);
            bridge.stop();
            println!("Bridge 48k->384k stopped cleanly.");
        }
        unsafe { CoUninitialize() };
    }

    #[test]
    fn test_diagnose_brave_routing_and_active_sessions() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let devices = enumerate_render_devices().unwrap();
        let sessions = crate::audio::enumerator::get_active_app_sessions(&devices);
        println!("\n=== ACTIVE AUDIO SESSIONS ===");
        for s in &sessions {
            println!("App: [{}] | PID: {} | Current Device: [{}]", s.name, s.pid, s.current_device_id);
        }
        println!("=============================");

        let realtek = devices.iter().find(|d| d.name.contains("Realtek")).unwrap();
        let cx = devices.iter().find(|d| d.name.contains("CX31993")).unwrap();
        println!("Starting 1.5s live monitor from [{}] to [{}]...", realtek.name, cx.name);
        let mut bridge = DualOutputBridge::start(0, realtek.id.clone(), cx.id.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let lat = f32::from_bits(bridge.latency_ms.load(Ordering::Acquire));
        let drift = bridge.drift_ppm.load(Ordering::Acquire);
        println!("Bridge monitor complete: latency = {:.2} ms, drift = {} ppm", lat, drift);
        bridge.stop();

        unsafe { CoUninitialize() };
    }

    #[test]
    fn test_process_loopback_activation() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

        let devices = enumerate_render_devices().unwrap();
        let sessions = crate::audio::enumerator::get_active_app_sessions(&devices);
        let target_pid = sessions.first().map(|s| s.pid).unwrap_or(std::process::id());

        println!("Testing activate_process_loopback_client for PID: {}", target_pid);
        match crate::audio::capture::activate_process_loopback_client(target_pid) {
            Ok(client) => {
                println!("Successfully activated process loopback IAudioClient for PID {}", target_pid);
                
                let wfx_float = crate::audio::capture::create_waveformat_float_stereo(48000);
                let pwfx = &wfx_float as *const _ as *const windows::Win32::Media::Audio::WAVEFORMATEX;
                let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
                let init_res = unsafe {
                    client.Initialize(
                        AUDCLNT_SHAREMODE_SHARED,
                        stream_flags,
                        100_000,
                        0,
                        pwfx,
                        None,
                    )
                };
                println!("Initialize with 48kHz float stereo: {:?}", init_res);

                let event = unsafe { CreateEventW(None, false, false, None).unwrap() };
                let set_evt_res = unsafe { client.SetEventHandle(event) };
                println!("SetEventHandle: {:?}", set_evt_res);

                let cap_res: windows::core::Result<windows::Win32::Media::Audio::IAudioCaptureClient> = unsafe { client.GetService() };
                println!("GetService<IAudioCaptureClient>: {:?}", cap_res.is_ok());

                let start_res = unsafe { client.Start() };
                println!("AudioClient Start: {:?}", start_res);

                if let Ok(cap_client) = cap_res {
                    let wait_res = unsafe { WaitForSingleObject(event, 200) };
                    println!("WaitForSingleObject: {:?}", wait_res);

                    let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
                    let mut num_frames_read: u32 = 0;
                    let mut flags: u32 = 0;
                    let get_buf_res = unsafe {
                        cap_client.GetBuffer(
                            &mut buffer_ptr,
                            &mut num_frames_read,
                            &mut flags,
                            None,
                            None,
                        )
                    };
                    println!("GetBuffer result: {:?}, frames: {}", get_buf_res, num_frames_read);
                    if num_frames_read > 0 {
                        let _ = unsafe { cap_client.ReleaseBuffer(num_frames_read) };
                    }
                }

                let _ = unsafe { client.Stop() };
                let _ = unsafe { CloseHandle(event) };
            }
            Err(e) => {
                println!("Process loopback activation result for PID {}: {:?}", target_pid, e);
            }
        }

        unsafe { CoUninitialize() };
    }

    #[test]
    fn test_live_process_specific_dual_bridge() {
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        let devices = enumerate_render_devices().unwrap();
        let sessions = crate::audio::enumerator::get_active_app_sessions(&devices);
        
        let realtek = devices.iter().find(|d| d.name.contains("Realtek"));
        let cx = devices.iter().find(|d| d.name.contains("CX31993"));

        if let (Some(r), Some(c), Some(s)) = (realtek, cx, sessions.first()) {
            println!("\n=== LIVE PROCESS-SPECIFIC DUAL BRIDGE TEST ===");
            println!("Target App: [{}] | PID: {}", s.name, s.pid);
            println!("Bridge 1: PID {} -> [{}] -> [{}] (CX31993 384k -> Realtek 48k)", s.pid, c.name, r.name);

            let mut bridge1 = DualOutputBridge::start(s.pid, c.id.clone(), r.id.clone())
                .expect("Failed to start process-specific bridge 1");

            for i in 0..6 {
                std::thread::sleep(Duration::from_millis(500));
                let lat = f32::from_bits(bridge1.latency_ms.load(Ordering::Acquire));
                let drift = bridge1.drift_ppm.load(Ordering::Acquire);
                let underruns = bridge1.underrun_count.load(Ordering::Acquire);
                let slews = bridge1.slew_trigger_count.load(Ordering::Acquire);
                let fsm_state = bridge1.get_dsp_state();
                println!(
                    "Bridge CX31993->Realtek [sample {}]: fsm_state = {:?}, rb_fill_ms = {:.2} ms, underruns = {}, drift_ppm = {} ppm, slews = {}",
                    i, fsm_state, lat, underruns, drift, slews
                );
                // Assert initial latency is stable around 40ms (<= 55ms, down radically from 119ms)
                assert!(
                    lat <= 55.0,
                    "Bridge 1 latency exceeded 55ms target (expected ~40ms single-budget): got {:.2} ms",
                    lat
                );
                // Assert drift is strictly bounded within ±1500 ppm (Zone 2 maximum)
                assert!(
                    drift.abs() <= 1500,
                    "Bridge 1 drift exceeded ±1500 ppm bound: got {} ppm",
                    drift
                );
            }
            bridge1.stop();

            println!("\nBridge 2: PID {} -> [{}] -> [{}] (Realtek 48k -> CX31993 384k)", s.pid, r.name, c.name);
            let mut bridge2 = DualOutputBridge::start(s.pid, r.id.clone(), c.id.clone())
                .expect("Failed to start process-specific bridge 2");

            for i in 0..6 {
                std::thread::sleep(Duration::from_millis(500));
                let lat = f32::from_bits(bridge2.latency_ms.load(Ordering::Acquire));
                let drift = bridge2.drift_ppm.load(Ordering::Acquire);
                let underruns = bridge2.underrun_count.load(Ordering::Acquire);
                let slews = bridge2.slew_trigger_count.load(Ordering::Acquire);
                let fsm_state = bridge2.get_dsp_state();
                println!(
                    "Bridge Realtek->CX31993 [sample {}]: fsm_state = {:?}, rb_fill_ms = {:.2} ms, underruns = {}, drift_ppm = {} ppm, slews = {}",
                    i, fsm_state, lat, underruns, drift, slews
                );
                assert!(
                    lat <= 55.0,
                    "Bridge 2 latency exceeded 55ms target (expected ~40ms single-budget): got {:.2} ms",
                    lat
                );
                assert!(
                    drift.abs() <= 1500,
                    "Bridge 2 drift exceeded ±1500 ppm bound: got {} ppm",
                    drift
                );
            }
            bridge2.stop();
            println!("==============================================\n");
        }
        unsafe { CoUninitialize() };
    }
}

