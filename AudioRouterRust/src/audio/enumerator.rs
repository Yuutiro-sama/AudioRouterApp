//! Audio Device and Application Session Enumeration Module.
//!
//! Queries active audio endpoints and per-application audio sessions
//! via WASAPI and Windows Core Audio Interfaces.

use std::collections::HashMap;
use std::path::Path;
use windows::core::{Interface, HSTRING};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::Media::Audio::Endpoints::IAudioEndpointVolume;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, DEVICE_STATE_ACTIVE, IAudioSessionControl2,
    IAudioSessionManager2, IMMDevice, IMMDeviceCollection,
    IMMDeviceEnumerator, ISimpleAudioVolume, MMDeviceEnumerator, AudioSessionStateActive,
    AudioSessionStateExpired,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};

/// Information about a physical or virtual audio endpoint.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AudioDevice {
    pub id: String,
    pub name: String,
}

/// Information about an application currently holding an audio session.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AppAudioSession {
    pub pid: u32,
    pub name: String,
    pub exe_path: Option<String>,
    pub volume: f32,
    pub muted: bool,
    pub current_device_id: String,
}

impl From<AudioDevice> for crate::ui::popup::DeviceItem {
    fn from(d: AudioDevice) -> Self {
        Self {
            id: d.id,
            name: d.name,
            is_default: false,
            is_enabled: false,
            volume: 1.0,
            is_muted: false,
            latency_ms: 0.0,
            drift_ppm: 0.0,
        }
    }
}

impl From<AppAudioSession> for crate::ui::popup::AppAudioItem {
    fn from(s: AppAudioSession) -> Self {
        Self {
            pid: s.pid,
            name: s.name,
            exe_path: s.exe_path,
            current_device_id: s.current_device_id,
            volume: s.volume,
            is_muted: s.muted,
        }
    }
}

/// Enumerates all currently active render (output) audio devices.
pub fn enumerate_render_devices() -> windows::core::Result<Vec<AudioDevice>> {
    enumerate_devices_by_flow(eRender)
}

/// Enumerates all currently active capture (input) audio devices.
pub fn enumerate_capture_devices() -> windows::core::Result<Vec<AudioDevice>> {
    enumerate_devices_by_flow(eCapture)
}

/// Enumerates active devices for a given data flow direction.
fn enumerate_devices_by_flow(
    flow: windows::Win32::Media::Audio::EDataFlow,
) -> windows::core::Result<Vec<AudioDevice>> {
    let mut devices = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let collection: IMMDeviceCollection =
            enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)?;
        let count = collection.GetCount()?;

        for i in 0..count {
            if let Ok(device) = collection.Item(i) {
                if let Ok(id_pwstr) = device.GetId() {
                    let id = id_pwstr.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(id_pwstr.as_ptr() as *const _));

                    let mut name = String::new();
                    if let Ok(store) = device.OpenPropertyStore(STGM_READ) {
                        if let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) {
                            name = prop.to_string();
                        }
                    }
                    if name.trim().is_empty() {
                        name = format!("Audio Device ({})", &id[..std::cmp::min(12, id.len())]);
                    }

                    devices.push(AudioDevice { id, name });
                }
            }
        }
    }
    Ok(devices)
}

/// Gets the system default render device.
pub fn get_default_render_device() -> windows::core::Result<Option<AudioDevice>> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        if let Ok(device) = enumerator.GetDefaultAudioEndpoint(eRender, eConsole) {
            if let Ok(id_pwstr) = device.GetId() {
                let id = id_pwstr.to_string().unwrap_or_default();
                CoTaskMemFree(Some(id_pwstr.as_ptr() as *const _));

                let mut name = String::new();
                if let Ok(store) = device.OpenPropertyStore(STGM_READ) {
                    if let Ok(prop) = store.GetValue(&PKEY_Device_FriendlyName) {
                        name = prop.to_string();
                    }
                }
                if name.trim().is_empty() {
                    name = "Default Output Device".to_string();
                }
                return Ok(Some(AudioDevice { id, name }));
            }
        }
    }
    Ok(None)
}

/// Helper to get the executable path and display name of a process by PID.
fn get_process_info(pid: u32) -> (String, Option<String>) {
    unsafe {
        if let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            let mut buf = [0u16; 1024];
            let mut len = buf.len() as u32;
            let pwstr = windows::core::PWSTR(buf.as_mut_ptr());

            if QueryFullProcessImageNameW(handle, PROCESS_NAME_WIN32, pwstr, &mut len).is_ok() {
                let _ = CloseHandle(handle);
                let full_path = String::from_utf16_lossy(&buf[..len as usize]);
                let file_name = Path::new(&full_path)
                    .file_name()
                    .map(|f| f.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("PID: {}", pid));
                return (file_name, Some(full_path));
            }
            let _ = CloseHandle(handle);
        }
    }
    (format!("Process ({})", pid), None)
}

/// Queries all active application audio sessions across the provided list of render devices.
///
/// Sessions with `pid == 0` (system sound / idle) or in `AudioSessionStateExpired` are filtered out.
/// Duplicate sessions for the same PID are consolidated into a single entry.
pub fn get_active_app_sessions(devices: &[AudioDevice]) -> Vec<AppAudioSession> {
    let mut session_map: HashMap<u32, AppAudioSession> = HashMap::new();

    let local_devices;
    let target_devices = if devices.is_empty() {
        local_devices = enumerate_render_devices().unwrap_or_default();
        &local_devices[..]
    } else {
        devices
    };

    unsafe {
        let enumerator: Result<IMMDeviceEnumerator, _> =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL);
        let Ok(enumerator) = enumerator else {
            return Vec::new();
        };

        for dev in target_devices {
            let hstring_id = HSTRING::from(&dev.id);
            let device: Result<IMMDevice, _> = enumerator.GetDevice(&hstring_id);
            let Ok(device) = device else {
                continue;
            };

            let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
                continue;
            };

            let Ok(session_enum) = manager.GetSessionEnumerator() else {
                continue;
            };

            let count = session_enum.GetCount().unwrap_or(0);
            for s in 0..count {
                let Ok(control) = session_enum.GetSession(s) else {
                    continue;
                };

                // Check session state: skip expired sessions
                if let Ok(state) = control.GetState() {
                    if state == AudioSessionStateExpired {
                        continue;
                    }
                }

                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };

                let Ok(pid) = control2.GetProcessId() else {
                    continue;
                };

                // Skip system-level session
                if pid == 0 {
                    continue;
                }

                // Query volume & mute status
                let mut volume = 1.0f32;
                let mut muted = false;
                if let Ok(sav) = control.cast::<ISimpleAudioVolume>() {
                    if let Ok(v) = sav.GetMasterVolume() {
                        volume = v;
                    }
                    if let Ok(m) = sav.GetMute() {
                        muted = m.as_bool();
                    }
                }

                let is_active = control.GetState().map(|s| s == AudioSessionStateActive).unwrap_or(false);

                if let Some(existing) = session_map.get_mut(&pid) {
                    if is_active {
                        existing.current_device_id = dev.id.clone();
                    }
                    existing.volume = volume;
                    existing.muted = muted;
                } else {
                    let (name, exe_path) = get_process_info(pid);
                    session_map.insert(
                        pid,
                        AppAudioSession {
                            pid,
                            name,
                            exe_path,
                            volume,
                            muted,
                            current_device_id: dev.id.clone(),
                        },
                    );
                }
            }
        }
    }

    // Query explicit persisted endpoint from Windows Audio Policy if registered
    if let Ok(factory) = crate::audio::policy_config::get_policy_config_factory() {
        for session in session_map.values_mut() {
            if let Ok(Some(persisted_id)) = crate::audio::policy_config::get_app_output_device(
                &factory,
                session.pid,
                eRender,
                windows::Win32::Media::Audio::eMultimedia,
            ) {
                session.current_device_id = persisted_id;
            }
        }
    }

    let mut sessions: Vec<AppAudioSession> = session_map.into_values().collect();
    sessions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    sessions
}

/// Queries all process IDs matching a given process/executable name (case-insensitive).
pub fn get_pids_by_process_name(name: &str) -> Vec<u32> {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
    };

    let mut pids = Vec::new();
    let name_lower = name.to_lowercase();
    let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);

    unsafe {
        if let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            let mut entry = PROCESSENTRY32W {
                dwSize: core::mem::size_of::<PROCESSENTRY32W>() as u32,
                ..Default::default()
            };
            if Process32FirstW(snapshot, &mut entry).is_ok() {
                loop {
                    let exe_name = String::from_utf16_lossy(&entry.szExeFile);
                    let exe_trimmed = exe_name.trim_matches('\0').to_lowercase();
                    let exe_no_ext = exe_trimmed.strip_suffix(".exe").unwrap_or(&exe_trimmed);
                    if exe_no_ext == name_no_ext {
                        pids.push(entry.th32ProcessID);
                    }
                    if Process32NextW(snapshot, &mut entry).is_err() {
                        break;
                    }
                }
            }
            let _ = CloseHandle(snapshot);
        }
    }
    pids
}

/// Sets the volume level for a specific application audio session by PID.
pub fn set_app_volume(pid: u32, device_id: &str, volume: f32) -> windows::core::Result<bool> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let hstring_id = HSTRING::from(device_id);
        let device = enumerator.GetDevice(&hstring_id)?;
        let manager = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)?;
        let session_enum = manager.GetSessionEnumerator()?;
        let count = session_enum.GetCount()?;

        for s in 0..count {
            if let Ok(control) = session_enum.GetSession(s) {
                if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                    if let Ok(p) = control2.GetProcessId() {
                        if p == pid {
                            if let Ok(sav) = control.cast::<ISimpleAudioVolume>() {
                                let clamped_vol = volume.clamp(0.0f32, 1.0f32);
                                sav.SetMasterVolume(clamped_vol, std::ptr::null())?;
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Sets the mute status for a specific application audio session by PID.
pub fn set_app_mute(pid: u32, device_id: &str, mute: bool) -> windows::core::Result<bool> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let hstring_id = HSTRING::from(device_id);
        let device = enumerator.GetDevice(&hstring_id)?;
        let manager = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None)?;
        let session_enum = manager.GetSessionEnumerator()?;
        let count = session_enum.GetCount()?;

        for s in 0..count {
            if let Ok(control) = session_enum.GetSession(s) {
                if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                    if let Ok(p) = control2.GetProcessId() {
                        if p == pid {
                            if let Ok(sav) = control.cast::<ISimpleAudioVolume>() {
                                sav.SetMute(
                                    windows::Win32::Foundation::BOOL::from(mute),
                                    std::ptr::null(),
                                )?;
                                return Ok(true);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

/// Sets the master hardware/endpoint volume for a physical/virtual audio device.
pub fn set_device_endpoint_volume(device_id: &str, volume: f32) -> windows::core::Result<bool> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let hstring_id = HSTRING::from(device_id);
        let device = enumerator.GetDevice(&hstring_id)?;
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            let clamped = volume.clamp(0.0, 1.0);
            endpoint_vol.SetMasterVolumeLevelScalar(clamped, std::ptr::null())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Sets the mute status on the hardware/endpoint of a physical/virtual audio device.
pub fn set_device_endpoint_mute(device_id: &str, mute: bool) -> windows::core::Result<bool> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let hstring_id = HSTRING::from(device_id);
        let device = enumerator.GetDevice(&hstring_id)?;
        if let Ok(endpoint_vol) = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) {
            endpoint_vol.SetMute(windows::Win32::Foundation::BOOL::from(mute), std::ptr::null())?;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Gets the master hardware/endpoint volume and mute status for a device.
pub fn get_device_endpoint_volume(device_id: &str) -> Option<(f32, bool)> {
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
        let hstring_id = HSTRING::from(device_id);
        let device = enumerator.GetDevice(&hstring_id).ok()?;
        let endpoint_vol = device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None).ok()?;
        let vol = endpoint_vol.GetMasterVolumeLevelScalar().unwrap_or(1.0);
        let mute = endpoint_vol.GetMute().map(|b| b.as_bool()).unwrap_or(false);
        Some((vol, mute))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_device_conversion_to_ui_item() {
        let dev = AudioDevice {
            id: "{0.0.0.00000000}.{test-id}".to_string(),
            name: "Headphones".to_string(),
        };
        let ui_item: crate::ui::popup::DeviceItem = dev.clone().into();
        assert_eq!(ui_item.id, dev.id);
        assert_eq!(ui_item.name, dev.name);
    }

    #[test]
    fn test_app_session_conversion_to_ui_item() {
        let session = AppAudioSession {
            pid: 1234,
            name: "spotify.exe".to_string(),
            exe_path: Some("C:\\spotify.exe".to_string()),
            volume: 0.75,
            muted: false,
            current_device_id: "{0.0.0.00000000}.{test-id}".to_string(),
        };
        let ui_item: crate::ui::popup::AppAudioItem = session.clone().into();
        assert_eq!(ui_item.pid, session.pid);
        assert_eq!(ui_item.name, session.name);
        assert_eq!(ui_item.volume, session.volume);
        assert_eq!(ui_item.is_muted, session.muted);
    }
}
