use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

/// Application route rule configuration (legacy compatibility)
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

/// 100% compatible configuration container for legacy `%AppData%\AudioRouter\rules.json`
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

/// Chuẩn hóa tên ứng dụng (chữ thường, hậu tố .exe)
pub fn normalize_app_key(app_name: &str) -> String {
    let lower = app_name.trim().to_lowercase();
    if lower.ends_with(".exe") {
        lower
    } else {
        format!("{}.exe", lower)
    }
}

/// Version 3.0.0 Multi-Output and Application Configuration
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
pub struct ConfigV3 {
    /// Danh sách ID các thiết bị đầu ra được chọn để phát đồng thời.
    #[serde(default)]
    pub enabled_devices: Vec<String>,
    /// Âm lượng độc lập cho từng thiết bị (0.0 .. 1.0)
    #[serde(default)]
    pub device_volumes: BTreeMap<String, f32>,
    /// Trạng thái tắt tiếng từng thiết bị (true = tắt tiếng)
    #[serde(default)]
    pub device_mutes: BTreeMap<String, bool>,
    /// Quy tắc định tuyến ghim cứng của từng app hoặc "multi"
    #[serde(default)]
    pub app_rules: BTreeMap<String, String>,
    /// Âm lượng lưu của từng app (0.0 .. 1.0)
    #[serde(default)]
    pub app_volumes: BTreeMap<String, f32>,
    /// Trạng thái mute của từng app (true = tắt tiếng)
    #[serde(default)]
    pub app_mutes: BTreeMap<String, bool>,
}

impl ConfigV3 {
    pub fn new() -> Self {
        Self::default()
    }

    /// Khởi tạo cấu hình mặc định: kích hoạt thiết bị mặc định với âm lượng 1.0 (100%)
    pub fn init_default(default_device_id: Option<&str>) -> Self {
        let mut cfg = Self::default();
        if let Some(dev_id) = default_device_id {
            if !dev_id.is_empty() {
                cfg.enabled_devices.push(dev_id.to_string());
                cfg.device_volumes.insert(dev_id.to_string(), 1.0);
                cfg.device_mutes.insert(dev_id.to_string(), false);
            }
        }
        cfg
    }

    /// Kiểm tra thiết bị có đang được kích hoạt phát đồng thời không
    pub fn is_device_enabled(&self, device_id: &str) -> bool {
        self.enabled_devices.iter().any(|d| d == device_id)
    }

    /// Bật hoặc tắt phát đồng thời cho một thiết bị
    pub fn set_device_enabled(&mut self, device_id: String, enabled: bool) {
        if enabled {
            if !self.is_device_enabled(&device_id) {
                self.enabled_devices.push(device_id);
            }
        } else {
            self.enabled_devices.retain(|d| d != &device_id);
        }
    }

    /// Lấy âm lượng của thiết bị (mặc định 1.0 nếu chưa thiết lập)
    pub fn get_device_volume(&self, device_id: &str) -> f32 {
        self.device_volumes.get(device_id).copied().unwrap_or(1.0)
    }

    /// Thiết lập âm lượng cho thiết bị (clamped 0.0 .. 1.0)
    pub fn set_device_volume(&mut self, device_id: String, volume: f32) {
        self.device_volumes.insert(device_id, volume.clamp(0.0, 1.0));
    }

    /// Kiểm tra trạng thái tắt tiếng của thiết bị (mặc định false)
    pub fn is_device_muted(&self, device_id: &str) -> bool {
        self.device_mutes.get(device_id).copied().unwrap_or(false)
    }

    /// Thiết lập trạng thái tắt tiếng cho thiết bị
    pub fn set_device_mute(&mut self, device_id: String, muted: bool) {
        self.device_mutes.insert(device_id, muted);
    }

    /// Tra cứu quy tắc định tuyến của app (case-insensitive & .exe tolerant)
    pub fn get_app_rule(&self, app_name: &str) -> Option<&String> {
        let norm = normalize_app_key(app_name);
        if let Some(val) = self.app_rules.get(&norm) {
            return Some(val);
        }
        let name_lower = app_name.to_lowercase();
        if let Some(val) = self.app_rules.get(&name_lower) {
            return Some(val);
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.app_rules {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return Some(v);
            }
        }
        None
    }

    /// Lưu quy tắc định tuyến cho ứng dụng
    pub fn set_app_rule(&mut self, app_name: String, target: String) {
        self.app_rules.insert(normalize_app_key(&app_name), target);
    }

    /// Xóa quy tắc định tuyến của ứng dụng
    pub fn remove_app_rule(&mut self, app_name: &str) -> Option<String> {
        let norm = normalize_app_key(app_name);
        self.app_rules.remove(&norm)
            .or_else(|| self.app_rules.remove(&app_name.to_lowercase()))
    }

    /// Lấy âm lượng của app (mặc định 1.0, case-insensitive & .exe tolerant)
    pub fn get_app_volume(&self, app_name: &str) -> f32 {
        let norm = normalize_app_key(app_name);
        if let Some(&vol) = self.app_volumes.get(&norm) {
            return vol;
        }
        let name_lower = app_name.to_lowercase();
        if let Some(&vol) = self.app_volumes.get(&name_lower) {
            return vol;
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.app_volumes {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return *v;
            }
        }
        1.0
    }

    /// Lưu âm lượng của app
    pub fn set_app_volume(&mut self, app_name: String, volume: f32) {
        self.app_volumes.insert(normalize_app_key(&app_name), volume.clamp(0.0, 1.0));
    }

    /// Kiểm tra trạng thái tắt tiếng của app (mặc định false)
    pub fn is_app_muted(&self, app_name: &str) -> bool {
        let norm = normalize_app_key(app_name);
        if let Some(&muted) = self.app_mutes.get(&norm) {
            return muted;
        }
        let name_lower = app_name.to_lowercase();
        if let Some(&muted) = self.app_mutes.get(&name_lower) {
            return muted;
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.app_mutes {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return *v;
            }
        }
        false
    }

    /// Lưu trạng thái tắt tiếng của app
    pub fn set_app_mute(&mut self, app_name: String, muted: bool) {
        self.app_mutes.insert(normalize_app_key(&app_name), muted);
    }
}

/// Thư mục cơ sở `%APPDATA%\AudioRouter`
pub fn get_appdata_dir() -> PathBuf {
    unsafe {
        if let Ok(pwstr) = windows::Win32::UI::Shell::SHGetKnownFolderPath(
            &windows::Win32::UI::Shell::FOLDERID_RoamingAppData,
            windows::Win32::UI::Shell::KF_FLAG_CREATE,
            None,
        ) {
            if let Ok(path_str) = pwstr.to_string() {
                windows::Win32::System::Com::CoTaskMemFree(Some(pwstr.as_ptr() as _));
                return PathBuf::from(path_str).join("AudioRouter");
            }
        }
    }

    if let Some(appdata) = std::env::var_os("APPDATA") {
        return PathBuf::from(appdata).join("AudioRouter");
    }

    if let Some(profile) = std::env::var_os("USERPROFILE") {
        return PathBuf::from(profile)
            .join("AppData")
            .join("Roaming")
            .join("AudioRouter");
    }

    PathBuf::from(".")
}

/// Returns the path to `%AppData%\AudioRouter\rules.json` (v2 legacy)
pub fn get_rules_path() -> PathBuf {
    get_appdata_dir().join("rules.json")
}

/// Returns the path to `%AppData%\AudioRouter\rules_v3.json` (v3.0.0)
pub fn get_config_v3_path() -> PathBuf {
    get_appdata_dir().join("rules_v3.json")
}

/// Tải cấu hình v3 từ `%AppData%\AudioRouter\rules_v3.json`.
/// Tự động nâng cấp / migration từ `rules.json` cũ nếu `rules_v3.json` chưa tồn tại.
pub fn load_config_v3() -> ConfigV3 {
    let path = get_config_v3_path();
    if path.exists() {
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(cfg) = serde_json::from_str::<ConfigV3>(&contents) {
                return cfg;
            }
        }
    }

    // Migration từ rules.json cũ nếu có
    let legacy_rules = load_rules();
    if !legacy_rules.is_empty() {
        let mut cfg = ConfigV3::default();
        cfg.app_rules = legacy_rules;
        let _ = save_config_v3(&cfg);
        return cfg;
    }

    ConfigV3::default()
}

/// Tải cấu hình v3 hoặc khởi tạo với thiết bị mặc định
pub fn load_or_init_config_v3(default_dev_id: Option<&str>) -> ConfigV3 {
    let mut cfg = load_config_v3();
    if cfg.enabled_devices.is_empty() {
        if let Some(id) = default_dev_id {
            if !id.is_empty() {
                cfg.enabled_devices.push(id.to_string());
                cfg.device_volumes.insert(id.to_string(), 1.0);
                cfg.device_mutes.insert(id.to_string(), false);
                let _ = save_config_v3(&cfg);
            }
        }
    }
    cfg
}

/// Atomically saves ConfigV3 to `%AppData%\AudioRouter\rules_v3.json`.
/// Uses a temporary file (.tmp), calls `sync_all` to flush buffers,
/// and atomically swaps the destination via `MoveFileExW`
/// (`MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`).
pub fn save_config_v3(config: &ConfigV3) -> std::io::Result<()> {
    let target_path = get_config_v3_path();
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = target_path.with_extension("tmp");

    // Serialize to indented JSON
    let json_data = serde_json::to_string_pretty(config)
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

/// Loads legacy rules mapping (`app_exe_name` -> `device_id`) from `%AppData%\AudioRouter\rules.json`.
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
pub fn save_rules(rules: &BTreeMap<String, String>) -> std::io::Result<()> {
    let target_path = get_rules_path();
    if let Some(parent) = target_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let tmp_path = target_path.with_extension("tmp");

    let json_data = serde_json::to_string_pretty(rules)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;

    {
        use std::io::Write;
        let mut file = std::fs::File::create(&tmp_path)?;
        file.write_all(json_data.as_bytes())?;
        file.sync_all()?;
    }

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

        let path_v3 = get_config_v3_path();
        assert!(path_v3.to_string_lossy().contains("AudioRouter"));
        assert!(path_v3.to_string_lossy().ends_with("rules_v3.json"));
    }

    #[test]
    fn test_config_v3_default_and_helpers() {
        let mut cfg = ConfigV3::init_default(Some("{0.0.0.00000000}.{speaker}"));
        assert!(cfg.is_device_enabled("{0.0.0.00000000}.{speaker}"));
        assert_eq!(cfg.get_device_volume("{0.0.0.00000000}.{speaker}"), 1.0);
        assert!(!cfg.is_device_muted("{0.0.0.00000000}.{speaker}"));

        cfg.set_device_volume("{0.0.0.00000000}.{speaker}".into(), 0.75);
        assert_eq!(cfg.get_device_volume("{0.0.0.00000000}.{speaker}"), 0.75);

        cfg.set_device_mute("{0.0.0.00000000}.{speaker}".into(), true);
        assert!(cfg.is_device_muted("{0.0.0.00000000}.{speaker}"));

        cfg.set_app_rule("spotify.exe".into(), "multi".into());
        assert_eq!(cfg.get_app_rule("Spotify"), Some(&"multi".to_string()));
        assert_eq!(cfg.get_app_rule("spotify.exe"), Some(&"multi".to_string()));

        cfg.set_app_volume("spotify.exe".into(), 0.85);
        assert_eq!(cfg.get_app_volume("spotify.exe"), 0.85);
        assert_eq!(cfg.get_app_volume("Spotify"), 0.85);

        cfg.set_app_mute("spotify.exe".into(), true);
        assert!(cfg.is_app_muted("spotify.exe"));
        assert!(cfg.is_app_muted("Spotify"));
    }

    #[test]
    fn test_config_v3_roundtrip() {
        let mut cfg = ConfigV3::new();
        cfg.enabled_devices.push("dev_1".into());
        cfg.enabled_devices.push("dev_2".into());
        cfg.device_volumes.insert("dev_1".into(), 0.5);
        cfg.device_volumes.insert("dev_2".into(), 0.8);
        cfg.device_mutes.insert("dev_2".into(), true);
        cfg.app_rules.insert("chrome.exe".into(), "multi".into());
        cfg.app_rules.insert("game.exe".into(), "dev_1".into());

        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let loaded: ConfigV3 = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn test_config_v3_atomic_save_and_load() {
        let mut cfg = load_config_v3();
        let test_dev = "{test-guid-unit-test}";
        cfg.set_device_enabled(test_dev.into(), true);
        cfg.set_device_volume(test_dev.into(), 0.42);

        let res = save_config_v3(&cfg);
        assert!(res.is_ok());

        let reloaded = load_config_v3();
        assert!(reloaded.is_device_enabled(test_dev));
        assert_eq!(reloaded.get_device_volume(test_dev), 0.42);

        // Cleanup
        let mut cleanup = reloaded;
        cleanup.set_device_enabled(test_dev.into(), false);
        cleanup.device_volumes.remove(test_dev);
        let _ = save_config_v3(&cleanup);
    }
}
