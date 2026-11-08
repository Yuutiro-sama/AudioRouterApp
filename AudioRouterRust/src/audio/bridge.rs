//! High-Performance Real-Time Dual-Output Audio Bridge.
//!
//! Captures loopback audio from a designated source render endpoint (Primary Device A)
//! and resamples & renders it into a designated target render endpoint (Secondary Device B)
//! with sub-1ms internal latency, 4-point Cubic Hermite interpolation, and PI clock drift compensation.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use windows::core::HSTRING;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::dsp::AdaptiveResamplerDsp;
use crate::audio::render::{set_mmcss_pro_audio, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT};
use crate::audio::ring_buffer::SpscRingBuffer;

/// Active bridge connecting a source output device to a secondary target device.
pub struct DualOutputBridge {
    pub source_device_id: String,
    pub target_device_id: String,
    pub volume_scalar_bits: Arc<AtomicU32>,
    pub muted: Arc<AtomicBool>,
    pub latency_ms: Arc<AtomicU32>,
    pub drift_ppm: Arc<AtomicI32>,
    stop_signal: Arc<AtomicBool>,
    capture_handle: Option<std::thread::JoinHandle<()>>,
    render_handle: Option<std::thread::JoinHandle<()>>,
}

impl DualOutputBridge {
    /// Spawns a new real-time bridge from `source_device_id` to `target_device_id`.
    /// If `source_device_id` is empty, captures from the default console render endpoint.
    pub fn start(source_device_id: String, target_device_id: String) -> Result<Self, windows::core::Error> {
        let ring_buffer = Arc::new(SpscRingBuffer::default());
        let stop_signal = Arc::new(AtomicBool::new(false));
        let volume_scalar_bits = Arc::new(AtomicU32::new(1.0f32.to_bits()));
        let muted = Arc::new(AtomicBool::new(false));
        let latency_ms = Arc::new(AtomicU32::new(0.0f32.to_bits()));
        let drift_ppm = Arc::new(AtomicI32::new(0));

        let source_rate = Arc::new(AtomicU32::new(0));

        // 1. Spawn Capture Thread
        let rb_cap = ring_buffer.clone();
        let stop_cap = stop_signal.clone();
        let src_id = source_device_id.clone();
        let src_rate_cap = source_rate.clone();

        let capture_handle = std::thread::Builder::new()
            .name(format!("BridgeCap-{}", &source_device_id[..source_device_id.len().min(8)]))
            .spawn(move || {
                let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
                let mmcss = set_mmcss_pro_audio();

                let res = capture_worker_loop(&src_id, &rb_cap, &src_rate_cap, &stop_cap);
                if let Err(e) = res {
                    eprintln!("Capture worker [{}] error: {:?}", src_id, e);
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
        let drift_atomic = drift_ppm.clone();
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
                    &drift_atomic,
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
            source_device_id,
            target_device_id,
            volume_scalar_bits,
            muted,
            latency_ms,
            drift_ppm,
            stop_signal,
            capture_handle: Some(capture_handle),
            render_handle: Some(render_handle),
        })
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
    source_id: &str,
    rb: &Arc<SpscRingBuffer>,
    source_rate: &Arc<AtomicU32>,
    stop_signal: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let device: IMMDevice = if source_id.is_empty() {
        unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? }
    } else {
        unsafe { enumerator.GetDevice(&HSTRING::from(source_id))? }
    };

    let audio_client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let pwfx = unsafe { audio_client.GetMixFormat()? };
    let sample_rate = unsafe { (*pwfx).nSamplesPerSec };
    let channels = unsafe { (*pwfx).nChannels as usize };
    let bits_per_sample = unsafe { (*pwfx).wBitsPerSample };

    source_rate.store(sample_rate, Ordering::Release);

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

    let stream_flags = AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK;
    let init_res = unsafe {
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
            let total_stereo_samples = (num_frames_read as usize) * 2;
            if stereo_staging.len() < total_stereo_samples {
                stereo_staging.resize(total_stereo_samples, 0.0f32);
            }

            if is_silent {
                for sample in &mut stereo_staging[..total_stereo_samples] {
                    *sample = 0.0;
                }
            } else if is_float && bits_per_sample == 32 {
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
            } else if bits_per_sample == 16 {
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
            } else {
                for sample in &mut stereo_staging[..total_stereo_samples] {
                    *sample = 0.0;
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

fn render_worker_loop(
    target_id: &str,
    rb: &Arc<SpscRingBuffer>,
    source_rate: &Arc<AtomicU32>,
    volume_bits: &Arc<AtomicU32>,
    muted_flag: &Arc<AtomicBool>,
    lat_atomic: &Arc<AtomicU32>,
    drift_atomic: &Arc<AtomicI32>,
    stop_signal: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let device: IMMDevice = unsafe { enumerator.GetDevice(&HSTRING::from(target_id))? };

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
        100_000 // 10ms
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

    let mut dsp = AdaptiveResamplerDsp::new(in_rate, sample_rate, 20);

    // Prime the buffer with silence to start stream smoothly
    unsafe {
        let initial_buf = render_client.GetBuffer(buffer_frame_count)?;
        let bytes_per_sample = (bits_per_sample as usize) / 8;
        std::ptr::write_bytes(initial_buf, 0, (buffer_frame_count as usize) * channels * bytes_per_sample);
        render_client.ReleaseBuffer(buffer_frame_count, 0)?;
        audio_client.Start()?;
    }

    let mut intermediate_f32: Vec<f32> = Vec::with_capacity((buffer_frame_count as usize) * channels);

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
            dsp = AdaptiveResamplerDsp::new(cur_in_rate, sample_rate, 20);
        }

        let target_vol = f32::from_bits(volume_bits.load(Ordering::Acquire));
        let muted = muted_flag.load(Ordering::Acquire);

        // Update real-time stats
        let avail_frames = rb.available_read() / 2;
        let latency = dsp.current_latency_ms(avail_frames);
        lat_atomic.store(latency.to_bits(), Ordering::Release);
        drift_atomic.store(dsp.last_drift_ppm(), Ordering::Release);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::enumerator::enumerate_render_devices;

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
                            let is_float = unsafe {
                                if tag == 3 {
                                    true
                                } else if tag == 0xFFFE {
                                    let ext = pwfx as *const WAVEFORMATEXTENSIBLE;
                                    let subformat = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
                                    subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
                                } else {
                                    false
                                }
                            };
                            println!(
                                "Device: [{}] -> Tag: 0x{:X}, Rate: {} Hz, Ch: {}, Bits: {}, Float: {}",
                                d.name, tag, rate, ch, bits, is_float
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
            println!("Testing 48kHz -> 384kHz Bridge: [{}] -> [{}]", &r.name, &c.name);
            let mut bridge = DualOutputBridge::start(r.id.clone(), c.id.clone()).expect("Failed to start bridge");
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
        let mut bridge = DualOutputBridge::start(realtek.id.clone(), cx.id.clone()).unwrap();
        std::thread::sleep(Duration::from_millis(1500));
        let lat = f32::from_bits(bridge.latency_ms.load(Ordering::Acquire));
        let drift = bridge.drift_ppm.load(Ordering::Acquire);
        println!("Bridge monitor complete: latency = {:.2} ms, drift = {} ppm", lat, drift);
        bridge.stop();

        unsafe { CoUninitialize() };
    }

}

