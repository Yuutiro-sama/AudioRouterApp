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
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
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
    #[serde(default)]
    pub sibling_pids: Vec<u32>,
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

/// Raw audio session info collected before deduplication and filtering.
#[derive(Debug, Clone, PartialEq)]
pub struct CandidateSession {
    pub pid: u32,
    pub name: String,
    pub exe_path: Option<String>,
    pub volume: f32,
    pub muted: bool,
    pub device_id: String,
    pub is_active: bool,
}

impl CandidateSession {
    pub fn new(
        pid: u32,
        name: impl Into<String>,
        device_id: impl Into<String>,
        is_active: bool,
    ) -> Self {
        Self {
            pid,
            name: name.into(),
            exe_path: None,
            volume: 1.0,
            muted: false,
            device_id: device_id.into(),
            is_active,
        }
    }
}

/// Checks if a session belongs to the AudioRouter application itself or matches current process ID.
pub fn is_self_process(pid: u32, name: &str, exe_path: Option<&str>, current_pid: u32) -> bool {
    if pid == current_pid {
        return true;
    }
    let lower_name = name.trim().to_lowercase();
    if lower_name.contains("audiorouter") || lower_name.contains("audio_router") {
        return true;
    }
    if let Some(path) = exe_path {
        let lower_path = path.trim().to_lowercase();
        if lower_path.contains("audiorouter") || lower_path.contains("audio_router") {
            return true;
        }
    }
    false
}

/// Normalizes executable name to a deduplication key (case-insensitive without .exe suffix).
/// Uses PID for unnamed fallback sessions like "Process (1234)".
pub fn get_app_dedup_key(name: &str, pid: u32) -> String {
    let lower = name.trim().to_lowercase();
    if lower.starts_with("process (") || lower.starts_with("pid:") {
        format!("pid:{}", pid)
    } else {
        lower.strip_suffix(".exe").unwrap_or(&lower).to_string()
    }
}

/// Determines whether the incoming session should take precedence over existing primary session.
fn should_replace_primary(existing: &CandidateSession, incoming: &CandidateSession) -> bool {
    // 1. AudioSessionStateActive has absolute priority over inactive sessions
    if incoming.is_active && !existing.is_active {
        return true;
    }
    if !incoming.is_active && existing.is_active {
        return false;
    }

    // 2. Unmuted session preferred over muted session
    if !incoming.muted && existing.muted {
        return true;
    }
    if incoming.muted && !existing.muted {
        return false;
    }

    // 3. User-customized volume (not 1.0) preferred over default
    let incoming_custom = (incoming.volume - 1.0).abs() > 0.001;
    let existing_custom = (existing.volume - 1.0).abs() > 0.001;
    if incoming_custom && !existing_custom {
        return true;
    }

    false
}

/// Checks if a session name indicates an orphaned ghost/zombie process without an active executable.
pub fn is_ghost_session(name: &str) -> bool {
    let lower = name.trim().to_lowercase();
    lower.starts_with("process (") || lower.starts_with("pid:")
}

/// Consolidates raw candidate sessions:
/// 1. Filters out ghost / terminated process sessions.
/// 2. Filters out self process (AudioRouter / current PID).
/// 3. Deduplicates sessions belonging to the same application (e.g. Discord, Chrome, Brave).
/// 4. Retains the active / primary session representation, tracks all sibling PIDs.
pub fn deduplicate_sessions(
    candidates: impl IntoIterator<Item = CandidateSession>,
    current_pid: u32,
) -> Vec<AppAudioSession> {
    let mut map: HashMap<String, (CandidateSession, AppAudioSession)> = HashMap::new();

    for candidate in candidates {
        // Ghost filtering: skip unidentifiable / terminated process sessions
        if is_ghost_session(&candidate.name) {
            continue;
        }

        // Self-filtering: skip current process or audiorouter executables
        if is_self_process(
            candidate.pid,
            &candidate.name,
            candidate.exe_path.as_deref(),
            current_pid,
        ) {
            continue;
        }

        let key = get_app_dedup_key(&candidate.name, candidate.pid);

        if let Some((existing_cand, existing_sess)) = map.get_mut(&key) {
            // Track all distinct PIDs belonging to this application
            if !existing_sess.sibling_pids.contains(&candidate.pid) {
                existing_sess.sibling_pids.push(candidate.pid);
            }

            if should_replace_primary(existing_cand, &candidate) {
                existing_sess.pid = candidate.pid;
                existing_sess.name = candidate.name.clone();
                if candidate.exe_path.is_some() {
                    existing_sess.exe_path = candidate.exe_path.clone();
                }
                existing_sess.volume = candidate.volume;
                existing_sess.muted = candidate.muted;
                existing_sess.current_device_id = candidate.device_id.clone();
                *existing_cand = candidate;
            } else if existing_sess.exe_path.is_none() && candidate.exe_path.is_some() {
                existing_sess.exe_path = candidate.exe_path.clone();
            }
        } else {
            let session = AppAudioSession {
                pid: candidate.pid,
                name: candidate.name.clone(),
                exe_path: candidate.exe_path.clone(),
                volume: candidate.volume,
                muted: candidate.muted,
                current_device_id: candidate.device_id.clone(),
                sibling_pids: vec![candidate.pid],
            };
            map.insert(key, (candidate, session));
        }
    }

    let mut sessions: Vec<AppAudioSession> = map.into_values().map(|(_, s)| s).collect();
    sessions.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    sessions
}

/// Queries all active application audio sessions across the provided list of render devices.
///
/// Sessions with `pid == 0` (system sound / idle) or in `AudioSessionStateExpired` are filtered out.
/// Self processes (AudioRouter or current PID) are strictly excluded.
/// Multiple sessions for multi-process apps (e.g. Discord, Chrome, Brave) are deduplicated into
/// exactly one entry per app, prioritizing active audio streams.
pub fn get_active_app_sessions(devices: &[AudioDevice]) -> Vec<AppAudioSession> {
    let current_pid = unsafe { GetCurrentProcessId() };

    let local_devices;
    let target_devices = if devices.is_empty() {
        local_devices = enumerate_render_devices().unwrap_or_default();
        &local_devices[..]
    } else {
        devices
    };

    let mut raw_candidates = Vec::new();

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

                // Skip system sound sessions (e.g. system notification chimes)
                if control2.IsSystemSoundsSession() == windows::Win32::Foundation::S_OK {
                    continue;
                }

                let Ok(pid) = control2.GetProcessId() else {
                    continue;
                };

                // Skip system-level session or current process
                if pid == 0 || pid == current_pid {
                    continue;
                }

                let (name, exe_path) = get_process_info(pid);

                // Skip ghost / zombie / unidentifiable sessions of terminated processes
                if exe_path.is_none() || is_ghost_session(&name) {
                    continue;
                }

                // Early filter for self process
                if is_self_process(pid, &name, exe_path.as_deref(), current_pid) {
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

                let is_active = control
                    .GetState()
                    .map(|s| s == AudioSessionStateActive)
                    .unwrap_or(false);

                raw_candidates.push(CandidateSession {
                    pid,
                    name,
                    exe_path,
                    volume,
                    muted,
                    device_id: dev.id.clone(),
                    is_active,
                });
            }
        }
    }

    let mut sessions = deduplicate_sessions(raw_candidates, current_pid);

    // Query explicit persisted endpoint from Windows Audio Policy if registered
    if let Ok(factory) = crate::audio::policy_config::get_policy_config_factory() {
        for session in &mut sessions {
            let mut found_persisted = None;
            if let Ok(Some(persisted_id)) = crate::audio::policy_config::get_app_output_device(
                &factory,
                session.pid,
                eRender,
                windows::Win32::Media::Audio::eMultimedia,
            ) {
                found_persisted = Some(persisted_id);
            } else {
                // If primary PID has no explicit policy, check sibling PIDs
                for &s_pid in &session.sibling_pids {
                    if s_pid != session.pid {
                        if let Ok(Some(persisted_id)) =
                            crate::audio::policy_config::get_app_output_device(
                                &factory,
                                s_pid,
                                eRender,
                                windows::Win32::Media::Audio::eMultimedia,
                            )
                        {
                            found_persisted = Some(persisted_id);
                            break;
                        }
                    }
                }
            }

            if let Some(id) = found_persisted {
                session.current_device_id = id;
            }
        }
    }

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

        let mut matched = false;
        for s in 0..count {
            if let Ok(control) = session_enum.GetSession(s) {
                if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                    if let Ok(p) = control2.GetProcessId() {
                        if p == pid {
                            if let Ok(sav) = control.cast::<ISimpleAudioVolume>() {
                                let clamped_vol = volume.clamp(0.0f32, 1.0f32);
                                let _ = sav.SetMasterVolume(clamped_vol, std::ptr::null());
                                matched = true;
                            }
                        }
                    }
                }
            }
        }
        Ok(matched)
    }
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

        let mut matched = false;
        for s in 0..count {
            if let Ok(control) = session_enum.GetSession(s) {
                if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                    if let Ok(p) = control2.GetProcessId() {
                        if p == pid {
                            if let Ok(sav) = control.cast::<ISimpleAudioVolume>() {
                                let _ = sav.SetMute(
                                    windows::Win32::Foundation::BOOL::from(mute),
                                    std::ptr::null(),
                                );
                                matched = true;
                            }
                        }
                    }
                }
            }
        }
        Ok(matched)
    }
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
            sibling_pids: vec![1234],
        };
        let ui_item: crate::ui::popup::AppAudioItem = session.clone().into();
        assert_eq!(ui_item.pid, session.pid);
        assert_eq!(ui_item.name, session.name);
        assert_eq!(ui_item.volume, session.volume);
        assert_eq!(ui_item.is_muted, session.muted);
    }

    #[test]
    fn test_filter_self_process() {
        let current_pid = 8888;
        let candidates = vec![
            CandidateSession {
                pid: current_pid,
                name: "AudioRouter.exe".to_string(),
                exe_path: Some("C:\\AudioRouter\\AudioRouter.exe".to_string()),
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
            CandidateSession {
                pid: 1111,
                name: "audio_router_rust.exe".to_string(),
                exe_path: None,
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 2222,
                name: "audiorouter-v3.exe".to_string(),
                exe_path: None,
                volume: 0.5,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 3333,
                name: "other_app.exe".to_string(),
                exe_path: Some("D:\\Tools\\audiorouter_helper\\other_app.exe".to_string()),
                volume: 0.5,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 4444,
                name: "spotify.exe".to_string(),
                exe_path: Some("C:\\Users\\AppData\\Spotify\\Spotify.exe".to_string()),
                volume: 0.8,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
            CandidateSession {
                pid: 5555,
                name: "firefox.exe".to_string(),
                exe_path: None,
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
        ];

        let result = deduplicate_sessions(candidates, current_pid);

        // AudioRouter, audio_router_rust, audiorouter-v3 and helper with audiorouter path must be filtered out
        assert_eq!(result.len(), 2);
        for item in &result {
            assert_ne!(item.pid, current_pid);
            let lower_name = item.name.to_lowercase();
            assert!(!lower_name.contains("audiorouter"));
            assert!(!lower_name.contains("audio_router"));
        }
        assert!(result.iter().any(|s| s.name.to_lowercase() == "spotify.exe"));
        assert!(result.iter().any(|s| s.name.to_lowercase() == "firefox.exe"));
    }

    #[test]
    fn test_deduplicate_app_sessions() {
        let current_pid = 9999;
        let candidates = vec![
            CandidateSession {
                pid: 1000,
                name: "Discord.exe".to_string(),
                exe_path: Some("C:\\Discord\\Discord.exe".to_string()),
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 2000,
                name: "discord.exe".to_string(),
                exe_path: Some("C:\\Discord\\Discord.exe".to_string()),
                volume: 0.65,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
            CandidateSession {
                pid: 3001,
                name: "brave.exe".to_string(),
                exe_path: Some("C:\\Brave\\brave.exe".to_string()),
                volume: 1.0,
                muted: true,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 3002,
                name: "Brave.exe".to_string(),
                exe_path: Some("C:\\Brave\\brave.exe".to_string()),
                volume: 0.8,
                muted: false,
                device_id: "dev2".to_string(),
                is_active: true,
            },
            CandidateSession {
                pid: 3003,
                name: "brave.exe".to_string(),
                exe_path: Some("C:\\Brave\\brave.exe".to_string()),
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 4000,
                name: "spotify.exe".to_string(),
                exe_path: None,
                volume: 0.9,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
        ];

        let result = deduplicate_sessions(candidates, current_pid);

        // Must consolidate down to exactly 3 unique applications
        assert_eq!(result.len(), 3);

        // Check Discord consolidation
        let discord = result
            .iter()
            .find(|s| s.name.to_lowercase() == "discord.exe")
            .expect("Discord must exist");
        assert_eq!(discord.pid, 2000, "Should prioritize the active session");
        assert_eq!(discord.volume, 0.65);
        assert_eq!(discord.muted, false);
        assert!(discord.sibling_pids.contains(&1000));
        assert!(discord.sibling_pids.contains(&2000));
        assert_eq!(discord.sibling_pids.len(), 2);

        // Check Brave consolidation (3 PIDs into 1)
        let brave = result
            .iter()
            .find(|s| s.name.to_lowercase() == "brave.exe")
            .expect("Brave must exist");
        assert_eq!(brave.pid, 3002, "Should prioritize the active unmuted session");
        assert_eq!(brave.volume, 0.8);
        assert_eq!(brave.muted, false);
        assert_eq!(brave.current_device_id, "dev2");
        assert!(brave.sibling_pids.contains(&3001));
        assert!(brave.sibling_pids.contains(&3002));
        assert!(brave.sibling_pids.contains(&3003));
        assert_eq!(brave.sibling_pids.len(), 3);

        // Verify order independence: Active session first, inactive session second
        let reverse_discord = vec![
            CandidateSession {
                pid: 2000,
                name: "Discord.exe".to_string(),
                exe_path: None,
                volume: 0.65,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
            CandidateSession {
                pid: 1000,
                name: "discord.exe".to_string(),
                exe_path: None,
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
        ];
        let reverse_res = deduplicate_sessions(reverse_discord, current_pid);
        assert_eq!(reverse_res.len(), 1);
        assert_eq!(reverse_res[0].pid, 2000);
        assert_eq!(reverse_res[0].volume, 0.65);
        assert!(reverse_res[0].sibling_pids.contains(&1000));
        assert!(reverse_res[0].sibling_pids.contains(&2000));
    }

    #[test]
    fn test_live_system_self_filter() {
        let current_pid = unsafe { GetCurrentProcessId() };
        let live_sessions = get_active_app_sessions(&[]);
        for s in &live_sessions {
            assert_ne!(
                s.pid, current_pid,
                "Current process PID should never be returned in active sessions"
            );
            let lower = s.name.to_lowercase();
            assert!(
                !lower.contains("audiorouter"),
                "Active sessions should never include audiorouter: {}",
                s.name
            );
            assert!(
                !lower.contains("audio_router"),
                "Active sessions should never include audio_router: {}",
                s.name
            );
        }

        // Verify no duplicate application names exist in the result
        let mut seen_apps = std::collections::HashSet::new();
        for s in &live_sessions {
            let key = get_app_dedup_key(&s.name, s.pid);
            assert!(
                seen_apps.insert(key.clone()),
                "Duplicate application session found in live result: {}",
                key
            );
        }
    }

    #[test]
    fn test_filter_ghost_and_system_sessions() {
        let current_pid = 7777;
        let candidates = vec![
            CandidateSession {
                pid: 11344,
                name: "Process (11344)".to_string(),
                exe_path: None,
                volume: 1.0,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 11345,
                name: "PID: 11345".to_string(),
                exe_path: None,
                volume: 0.8,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
            CandidateSession {
                pid: 11346,
                name: "process (8888)".to_string(),
                exe_path: None,
                volume: 0.5,
                muted: true,
                device_id: "dev2".to_string(),
                is_active: false,
            },
            CandidateSession {
                pid: 5000,
                name: "spotify.exe".to_string(),
                exe_path: Some("C:\\Spotify\\spotify.exe".to_string()),
                volume: 0.7,
                muted: false,
                device_id: "dev1".to_string(),
                is_active: true,
            },
        ];

        let result = deduplicate_sessions(candidates, current_pid);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].name, "spotify.exe");
        assert_eq!(result[0].pid, 5000);
        assert!(!result.iter().any(|s| is_ghost_session(&s.name)));
    }
}
