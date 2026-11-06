use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

/// Application route rule configuration
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppRouteRule {
    /// Normalized process executable name (e.g., "spotify.exe", "chrome.exe")
    pub exe_name: String,
    /// Preferred MMDevice ID for audio playback (eRender)
    pub preferred_render_id: Option<String>,
    /// Optional fallback device ID if preferred device is unavailable
    #[serde(default)]
    pub fallback_render_id: Option<String>,
    /// Optional preferred device ID for audio capture (eCapture)
    #[serde(default)]
    pub preferred_capture_id: Option<String>,
    /// Desired volume level from 0.0 to 1.0
    #[serde(default)]
    pub volume_level: Option<f32>,
    /// Whether the application audio session is muted
    #[serde(default)]
    pub is_muted: bool,
}

impl AppRouteRule {
    pub fn new(exe_name: impl Into<String>, preferred_render_id: Option<String>) -> Self {
        Self {
            exe_name: exe_name.into(),
            preferred_render_id,
            fallback_render_id: None,
            preferred_capture_id: None,
            volume_level: None,
            is_muted: false,
        }
    }
}

/// 100% compatible configuration container for `%AppData%\AudioRouter\rules.json`
/// Serializes to a flat JSON object: `{"spotify.exe": "{0.0.0.00000000}.{...}"}`
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(transparent)]
pub struct RulesConfig {
    pub rules: BTreeMap<String, String>,
}

impl RulesConfig {
    pub fn new() -> Self {
        Self {
            rules: BTreeMap::new(),
        }
    }

    pub fn get(&self, app_name: &str) -> Option<&String> {
        self.rules.get(app_name)
    }

    /// Case-insensitive, extension-tolerant lookup (matches "brave", "brave.exe", "Brave.exe").
    pub fn get_rule(&self, app_name: &str) -> Option<&String> {
        let name_lower = app_name.to_lowercase();
        if let Some(val) = self.rules.get(&name_lower) {
            return Some(val);
        }
        if let Some(val) = self.rules.get(app_name) {
            return Some(val);
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.rules {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return Some(v);
            }
        }
        None
    }

    pub fn insert(&mut self, app_name: String, device_id: String) {
        self.rules.insert(app_name, device_id);
    }

    pub fn remove(&mut self, app_name: &str) -> Option<String> {
        self.rules.remove(app_name)
    }
}

/// Returns the path to `%AppData%\AudioRouter\rules.json` using SHGetKnownFolderPath
/// with fallback to environment variables.
pub fn get_rules_path() -> PathBuf {
    // 1. Try SHGetKnownFolderPath with FOLDERID_RoamingAppData
    unsafe {
        if let Ok(pwstr) = windows::Win32::UI::Shell::SHGetKnownFolderPath(
            &windows::Win32::UI::Shell::FOLDERID_RoamingAppData,
            windows::Win32::UI::Shell::KF_FLAG_CREATE,
            None,
        ) {
            if let Ok(path_str) = pwstr.to_string() {
                windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as _));
                return PathBuf::from(path_str).join("AudioRouter").join("rules.json");
            }
        }
    }

    // 2. Fallback to %APPDATA%
    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("AudioRouter").join("rules.json");
    }

    // 3. Fallback to %USERPROFILE%\AppData\Roaming
    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Roaming")
            .join("AudioRouter")
            .join("rules.json");
    }

    PathBuf::from("rules.json")
}

/// Loads rules mapping (`app_exe_name` -> `device_id`) from `%AppData%\AudioRouter\rules.json`.
/// Guaranteed 100% compatible with the legacy C# AudioRouter rules store.
pub fn load_rules() -> BTreeMap<String, String> {
    let path = get_rules_path();
    if !path.exists() {
        return BTreeMap::new();
    }

    match std::fs::read_to_string(&path) {
        Ok(contents) => {
            // First attempt: standard C# format (Dictionary<string, string>)
            if let Ok(rules) = serde_json::from_str::<BTreeMap<String, String>>(&contents) {
                return rules;
            }

            // Fallback: structured rules format with AppRouteRule
            if let Ok(structured) = serde_json::from_str::<BTreeMap<String, AppRouteRule>>(&contents) {
                let mut map = BTreeMap::new();
                for (k, v) in structured {
                    if let Some(dev_id) = v.preferred_render_id {
                        map.insert(k, dev_id);
                    }
                }
                return map;
            }

            BTreeMap::new()
        }
        Err(_) => BTreeMap::new(),
    }
}

/// Atomically saves rules to `%AppData%\AudioRouter\rules.json`.
/// Uses a temporary file, calls `sync_all` (flush buffers), and atomically replaces
/// the destination via `MoveFileExW` (`MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`).
pub fn save_rules(rules: &BTreeMap<String, String>) -> std::io::Result<()> {
    let target_path = get_rules_path();
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = target_path.with_extension("tmp");

    // Serialize to indented JSON
    let json_data = serde_json::to_string_pretty(rules)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    // Write and flush to disk
    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(json_data.as_bytes())?;
        file.sync_all()?;
    }

    // Atomic swap via MoveFileExW
    let src_wide: Vec<u16> = tmp_path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let dst_wide: Vec<u16> = target_path.as_os_str().encode_wide().chain(std::iter::once(0)).collect();

    unsafe {
        let res = windows::Win32::Storage::FileSystem::MoveFileExW(
            windows::core::PCWSTR(src_wide.as_ptr()),
            windows::core::PCWSTR(dst_wide.as_ptr()),
            windows::Win32::Storage::FileSystem::MOVEFILE_REPLACE_EXISTING
                | windows::Win32::Storage::FileSystem::MOVEFILE_WRITE_THROUGH,
        );

        if res.is_err() {
            // Fallback to standard remove + rename if MoveFileExW fails
            let _ = std::fs::remove_file(&target_path);
            std::fs::rename(&tmp_path, &target_path)?;
        }
    }

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rules_path() {
        let path = get_rules_path();
        assert!(path.to_string_lossy().contains("AudioRouter"));
        assert!(path.to_string_lossy().ends_with("rules.json"));
    }

    #[test]
    fn test_app_route_rule_serde() {
        let rule = AppRouteRule::new("spotify.exe", Some("{0.0.0.00000000}.{test}".into()));
        let json = serde_json::to_string(&rule).unwrap();
        assert!(json.contains("spotify.exe"));
        let parsed: AppRouteRule = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.exe_name, "spotify.exe");
        assert_eq!(parsed.preferred_render_id, Some("{0.0.0.00000000}.{test}".into()));
    }

    #[test]
    fn test_rules_config_roundtrip() {
        let mut map = BTreeMap::new();
        map.insert("chrome.exe".to_string(), "dev_1".to_string());
        map.insert("vlc.exe".to_string(), "dev_2".to_string());

        let cfg = RulesConfig { rules: map.clone() };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let loaded: BTreeMap<String, String> = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, map);
    }

    #[test]
    fn test_atomic_save_and_load() {
        let mut rules = load_rules();
        let original_val = rules.get("audio_router_unit_test.exe").cloned();

        rules.insert("audio_router_unit_test.exe".into(), "{unit_test_guid}".into());
        let res = save_rules(&rules);
        assert!(res.is_ok());

        let reloaded = load_rules();
        assert_eq!(reloaded.get("audio_router_unit_test.exe"), Some(&"{unit_test_guid}".to_string()));

        // Restore
        let mut cleanup = reloaded;
        if let Some(orig) = original_val {
            cleanup.insert("audio_router_unit_test.exe".into(), orig);
        } else {
            cleanup.remove("audio_router_unit_test.exe");
        }
        let _ = save_rules(&cleanup);
    }
}
