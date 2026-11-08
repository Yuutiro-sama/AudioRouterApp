//! WASAPI Loopback Capture Engine & Multi-Target Fan-out.
//!
//! Captures master system audio using WASAPI loopback (`AUDCLNT_STREAMFLAGS_LOOPBACK`)
//! and fans out pristine stereo f32 PCM to all active `RenderTargetState` ring buffers.
//! Also provides structures for process-specific loopback (Windows 10 2004+ / Windows 11).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eConsole, eRender, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_LOOPBACK,
    IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, WAVEFORMATEXTENSIBLE,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::audio::render::{
    set_mmcss_pro_audio, RenderTargetState, KSDATAFORMAT_SUBTYPE_IEEE_FLOAT,
};

pub const VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK: PCWSTR =
    windows::core::w!("VAD\\Process_Loopback");

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcessLoopbackMode {
    IncludeTargetProcessTree = 0,
    ExcludeTargetProcessTree = 1,
}

#[repr(C)]
#[derive(Copy, Clone, Debug)]
pub struct AudioClientProcessLoopbackParams {
    pub target_process_id: u32,
    pub process_loopback_mode: ProcessLoopbackMode,
}

#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AudioClientActivationType {
    Default = 0,
    ProcessLoopback = 1,
}

#[repr(C)]
#[derive(Copy, Clone)]
pub struct AudioClientActivationParams {
    pub activation_type: AudioClientActivationType,
    pub process_loopback_params: AudioClientProcessLoopbackParams,
}

/// Spawns the master WASAPI loopback worker thread.
///
/// * `targets`: Shared list of output endpoints receiving the captured PCM stream.
/// * `capture_sample_rate`: Shared atomic to report the detected capture sample rate (e.g. 48000 Hz).
/// * `stop_signal`: Global stop signal for clean shutdown.
pub fn spawn_master_loopback_worker(
    targets: Arc<RwLock<Vec<Arc<RenderTargetState>>>>,
    capture_sample_rate: Arc<AtomicU32>,
    stop_signal: Arc<AtomicBool>,
) -> std::thread::JoinHandle<()> {
    std::thread::Builder::new()
        .name("WASAPI-MasterLoopback".to_string())
        .spawn(move || {
            // Enable Flush-to-Zero and Denormals-are-Zero
            #[cfg(target_arch = "x86_64")]
            #[allow(deprecated)]
            unsafe {
                use core::arch::x86_64::{_mm_getcsr, _mm_setcsr};
                let csr = _mm_getcsr();
                _mm_setcsr(csr | 0x8040);
            }

            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };

            let mmcss = set_mmcss_pro_audio();

            let res = loopback_capture_loop(&targets, &capture_sample_rate, &stop_signal);
            if let Err(e) = res {
                eprintln!("Master loopback worker stopped with error: {:?}", e);
            }

            if let Some((handle, rev_fn)) = mmcss {
                let _ = unsafe { rev_fn(handle) };
            }

            unsafe { CoUninitialize() };
        })
        .expect("Failed to spawn master loopback thread")
}

fn loopback_capture_loop(
    targets: &Arc<RwLock<Vec<Arc<RenderTargetState>>>>,
    capture_sample_rate: &Arc<AtomicU32>,
    stop_signal: &Arc<AtomicBool>,
) -> windows::core::Result<()> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    // WASAPI Loopback captures from the default playback endpoint
    let default_device: IMMDevice =
        unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole)? };

    let audio_client: IAudioClient =
        unsafe { default_device.Activate(CLSCTX_ALL, None)? };

    let pwfx = unsafe { audio_client.GetMixFormat()? };
    let sample_rate = unsafe { (*pwfx).nSamplesPerSec };
    let channels = unsafe { (*pwfx).nChannels as usize };
    let bits_per_sample = unsafe { (*pwfx).wBitsPerSample };

    capture_sample_rate.store(sample_rate, Ordering::Release);

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
            // Fallback without event callback (timer-driven loopback)
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

    let mut stereo_staging: Vec<f32> = Vec::with_capacity(4096);

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

            // Fan-out to all enabled render targets
            if let Ok(guard) = targets.read() {
                for target in guard.iter() {
                    if target.is_enabled() {
                        target
                            .ring_buffer
                            .push_slice(&stereo_staging[..total_stereo_samples]);
                    }
                }
            }

            let _ = unsafe { capture_client.ReleaseBuffer(num_frames_read) };
        }
    }

    let _ = unsafe { audio_client.Stop() };
    if use_event {
        let _ = unsafe { CloseHandle(event) };
    }

    Ok(())
}
