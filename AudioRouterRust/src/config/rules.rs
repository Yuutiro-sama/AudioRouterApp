use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;

fn default_volume() -> f32 {
    1.0
}

/// Version 3 per-application routing & volume rule
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct AppRuleV3 {
    #[serde(default)]
    pub primary_device: String,
    #[serde(default)]
    pub secondary_device: String,
    #[serde(default)]
    pub secondary_enabled: bool,
    #[serde(default = "default_volume")]
    pub volume: f32,
    #[serde(default)]
    pub muted: bool,
}

impl Default for AppRuleV3 {
    fn default() -> Self {
        Self {
            primary_device: String::new(),
            secondary_device: String::new(),
            secondary_enabled: false,
            volume: 1.0,
            muted: false,
        }
    }
}

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
    /// Cấu hình quy tắc của từng ứng dụng v3 (Primary, Secondary, Volume, Mute)
    #[serde(default)]
    pub apps: BTreeMap<String, AppRuleV3>,
    /// Quy tắc định tuyến ghim cứng của từng app hoặc "multi" (legacy compatibility)
    #[serde(default)]
    pub app_rules: BTreeMap<String, String>,
    /// Quy tắc thiết bị đầu ra phụ (secondary device) của từng app (legacy compatibility)
    #[serde(default)]
    pub app_secondary_rules: BTreeMap<String, String>,
    /// Trạng thái bật/tắt thiết bị đầu ra phụ của từng app (legacy compatibility)
    #[serde(default)]
    pub app_secondary_enabled: BTreeMap<String, bool>,
    /// Âm lượng lưu của từng app (0.0 .. 1.0) (legacy compatibility)
    #[serde(default)]
    pub app_volumes: BTreeMap<String, f32>,
    /// Trạng thái mute của từng app (true = tắt tiếng) (legacy compatibility)
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

    /// Tra cứu quy tắc định tuyến của app (case-insensitive & .exe tolerant).
    /// Ưu tiên tìm trong `apps`, nếu không có sẽ fallback về các trường legacy (`app_rules`, `app_secondary_rules`, `app_volumes`, `app_mutes`).
    pub fn get_app_rule(&self, app_name: &str) -> AppRuleV3 {
        let norm = normalize_app_key(app_name);
        if let Some(rule) = self.apps.get(&norm) {
            return rule.clone();
        }
        let name_lower = app_name.to_lowercase();
        if let Some(rule) = self.apps.get(&name_lower) {
            return rule.clone();
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.apps {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return v.clone();
            }
        }

        // Fallback kiểm tra các trường legacy
        let legacy_target = self.get_legacy_app_rule(app_name).cloned().unwrap_or_default();
        let sec_target = self.get_legacy_secondary_device(app_name).cloned().unwrap_or_default();
        let sec_enabled = self.is_legacy_secondary_enabled(app_name);

        let has_vol = self.app_volumes.contains_key(&norm)
            || self.app_volumes.contains_key(&name_lower)
            || self.app_volumes.iter().any(|(k, _)| {
                let kl = k.to_lowercase();
                kl.strip_suffix(".exe").unwrap_or(&kl) == name_no_ext
            });
        let has_mute = self.app_mutes.contains_key(&norm)
            || self.app_mutes.contains_key(&name_lower)
            || self.app_mutes.iter().any(|(k, _)| {
                let kl = k.to_lowercase();
                kl.strip_suffix(".exe").unwrap_or(&kl) == name_no_ext
            });

        if !legacy_target.is_empty() || !sec_target.is_empty() || sec_enabled || has_vol || has_mute {
            return AppRuleV3 {
                primary_device: legacy_target,
                secondary_device: sec_target,
                secondary_enabled: sec_enabled,
                volume: self.get_app_volume(app_name),
                muted: self.is_app_muted(app_name),
            };
        }

        AppRuleV3::default()
    }

    /// Tra cứu quy tắc legacy dạng chuỗi target device_id hoặc "multi"
    pub fn get_legacy_app_rule(&self, app_name: &str) -> Option<&String> {
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

    /// Tra cứu thiết bị đầu ra phụ dạng legacy (case-insensitive & .exe tolerant)
    pub fn get_legacy_secondary_device(&self, app_name: &str) -> Option<&String> {
        let norm = normalize_app_key(app_name);
        if let Some(val) = self.app_secondary_rules.get(&norm) {
            return Some(val);
        }
        let name_lower = app_name.to_lowercase();
        if let Some(val) = self.app_secondary_rules.get(&name_lower) {
            return Some(val);
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.app_secondary_rules {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return Some(v);
            }
        }
        None
    }

    /// Kiểm tra trạng thái bật/tắt thiết bị đầu ra phụ dạng legacy (mặc định false)
    pub fn is_legacy_secondary_enabled(&self, app_name: &str) -> bool {
        let norm = normalize_app_key(app_name);
        if let Some(&en) = self.app_secondary_enabled.get(&norm) {
            return en;
        }
        let name_lower = app_name.to_lowercase();
        if let Some(&en) = self.app_secondary_enabled.get(&name_lower) {
            return en;
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.app_secondary_enabled {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return *v;
            }
        }
        false
    }

    /// Tra cứu thiết bị đầu ra chính của app
    pub fn get_app_primary_device(&self, app_name: &str) -> Option<String> {
        let rule = self.get_app_rule(app_name);
        if !rule.primary_device.is_empty() {
            Some(rule.primary_device)
        } else {
            None
        }
    }

    /// Thiết lập thiết bị phát chính (Primary Device) cho ứng dụng
    pub fn set_app_primary_device(&mut self, app_name: String, dev_id: String) {
        let key = normalize_app_key(&app_name);
        let entry = self.apps.entry(key.clone()).or_insert_with(AppRuleV3::default);
        entry.primary_device = dev_id.clone();
        // Nếu thiết bị phụ trùng thiết bị chính vừa chọn, tự động xóa thiết bị phụ để tránh vòng lặp
        if !dev_id.is_empty() && entry.secondary_device == dev_id {
            entry.secondary_device.clear();
            entry.secondary_enabled = false;
            self.app_secondary_rules.remove(&key);
            self.app_secondary_enabled.insert(key.clone(), false);
        }
        // Đồng bộ trường legacy
        self.app_rules.insert(key, dev_id);
    }

    /// Tra cứu thiết bị đầu ra phụ của app
    pub fn get_app_secondary_device(&self, app_name: &str) -> Option<String> {
        let rule = self.get_app_rule(app_name);
        if !rule.secondary_device.is_empty() {
            Some(rule.secondary_device)
        } else {
            None
        }
    }

    /// Thiết lập thiết bị phát phụ (Secondary Device) cho ứng dụng
    pub fn set_app_secondary_device(&mut self, app_name: String, dev_id: String) {
        let key = normalize_app_key(&app_name);
        let entry = self.apps.entry(key.clone()).or_insert_with(AppRuleV3::default);
        // Không cho phép thiết bị phụ trùng với thiết bị chính
        if !entry.primary_device.is_empty() && entry.primary_device == dev_id {
            entry.secondary_device.clear();
            entry.secondary_enabled = false;
            self.app_secondary_rules.remove(&key);
            self.app_secondary_enabled.insert(key, false);
            return;
        }
        entry.secondary_device = dev_id.clone();
        if dev_id.is_empty() {
            entry.secondary_enabled = false;
            self.app_secondary_enabled.insert(key.clone(), false);
        }
        // Đồng bộ trường legacy
        self.app_secondary_rules.insert(key, dev_id);
    }

    /// Kiểm tra trạng thái bật/tắt thiết bị đầu ra phụ của app
    pub fn is_app_secondary_enabled(&self, app_name: &str) -> bool {
        self.get_app_rule(app_name).secondary_enabled
    }

    /// Bật/tắt phát đồng thời ra thiết bị phụ cho ứng dụng
    pub fn set_app_secondary_enabled(&mut self, app_name: String, enabled: bool) {
        let key = normalize_app_key(&app_name);
        let entry = self.apps.entry(key.clone()).or_insert_with(AppRuleV3::default);
        if enabled && (entry.secondary_device.is_empty() || (!entry.primary_device.is_empty() && entry.secondary_device == entry.primary_device)) {
            entry.secondary_enabled = false;
            self.app_secondary_enabled.insert(key, false);
            return;
        }
        entry.secondary_enabled = enabled;
        // Đồng bộ trường legacy
        self.app_secondary_enabled.insert(key, enabled);
    }

    /// Lưu quy tắc định tuyến cho ứng dụng (tương thích API cũ)
    pub fn set_app_rule(&mut self, app_name: String, target: String) {
        self.set_app_primary_device(app_name, target);
    }

    /// Xóa quy tắc định tuyến của ứng dụng
    pub fn remove_app_rule(&mut self, app_name: &str) -> Option<AppRuleV3> {
        let norm = normalize_app_key(app_name);
        let _ = self.app_rules.remove(&norm)
            .or_else(|| self.app_rules.remove(&app_name.to_lowercase()));
        let _ = self.app_secondary_rules.remove(&norm)
            .or_else(|| self.app_secondary_rules.remove(&app_name.to_lowercase()));
        let _ = self.app_secondary_enabled.remove(&norm)
            .or_else(|| self.app_secondary_enabled.remove(&app_name.to_lowercase()));
        self.apps.remove(&norm)
            .or_else(|| self.apps.remove(&app_name.to_lowercase()))
    }

    /// Lấy âm lượng của app (mặc định 1.0, case-insensitive & .exe tolerant)
    pub fn get_app_volume(&self, app_name: &str) -> f32 {
        let norm = normalize_app_key(app_name);
        if let Some(rule) = self.apps.get(&norm) {
            return rule.volume;
        }
        let name_lower = app_name.to_lowercase();
        if let Some(rule) = self.apps.get(&name_lower) {
            return rule.volume;
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.apps {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return v.volume;
            }
        }

        if let Some(&vol) = self.app_volumes.get(&norm) {
            return vol;
        }
        if let Some(&vol) = self.app_volumes.get(&name_lower) {
            return vol;
        }
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
        let clamped = volume.clamp(0.0, 1.0);
        let key = normalize_app_key(&app_name);
        let entry = self.apps.entry(key.clone()).or_insert_with(AppRuleV3::default);
        entry.volume = clamped;
        // Đồng bộ trường legacy app_volumes
        self.app_volumes.insert(key, clamped);
    }

    /// Kiểm tra trạng thái tắt tiếng của app (mặc định false)
    pub fn is_app_muted(&self, app_name: &str) -> bool {
        let norm = normalize_app_key(app_name);
        if let Some(rule) = self.apps.get(&norm) {
            return rule.muted;
        }
        let name_lower = app_name.to_lowercase();
        if let Some(rule) = self.apps.get(&name_lower) {
            return rule.muted;
        }
        let name_no_ext = name_lower.strip_suffix(".exe").unwrap_or(&name_lower);
        for (k, v) in &self.apps {
            let k_lower = k.to_lowercase();
            let k_no_ext = k_lower.strip_suffix(".exe").unwrap_or(&k_lower);
            if k_no_ext == name_no_ext {
                return v.muted;
            }
        }

        if let Some(&muted) = self.app_mutes.get(&norm) {
            return muted;
        }
        if let Some(&muted) = self.app_mutes.get(&name_lower) {
            return muted;
        }
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
        let key = normalize_app_key(&app_name);
        let entry = self.apps.entry(key.clone()).or_insert_with(AppRuleV3::default);
        entry.muted = muted;
        // Đồng bộ trường legacy app_mutes
        self.app_mutes.insert(key, muted);
    }

    /// Chuẩn hóa dữ liệu cấu hình, loại bỏ trường hợp secondary trùng primary hoặc secondary_enabled bật khi chưa chọn thiết bị phụ.
    pub fn sanitize(&mut self) {
        for (k, rule) in &mut self.apps {
            if !rule.primary_device.is_empty() && rule.primary_device == rule.secondary_device {
                rule.secondary_device.clear();
                rule.secondary_enabled = false;
                self.app_secondary_rules.remove(k);
                self.app_secondary_enabled.insert(k.clone(), false);
            } else if rule.secondary_device.is_empty() {
                rule.secondary_enabled = false;
                self.app_secondary_enabled.insert(k.clone(), false);
            }
        }
        for (k, sec_id) in &mut self.app_secondary_rules {
            if let Some(prim_id) = self.app_rules.get(k) {
                if !prim_id.is_empty() && prim_id == sec_id {
                    sec_id.clear();
                    self.app_secondary_enabled.insert(k.clone(), false);
                }
            }
        }
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
            if let Ok(mut cfg) = serde_json::from_str::<ConfigV3>(&contents) {
                // If apps is empty but legacy app_rules has entries, populate apps
                if cfg.apps.is_empty() && !cfg.app_rules.is_empty() {
                    for (k, v) in &cfg.app_rules {
                        let norm = normalize_app_key(k);
                        let sec_dev = cfg.app_secondary_rules.get(&norm).or_else(|| cfg.app_secondary_rules.get(k)).cloned().unwrap_or_default();
                        let sec_en = cfg.app_secondary_enabled.get(&norm).or_else(|| cfg.app_secondary_enabled.get(k)).copied().unwrap_or(false);
                        let vol = cfg.app_volumes.get(&norm).or_else(|| cfg.app_volumes.get(k)).copied().unwrap_or(1.0);
                        let muted = cfg.app_mutes.get(&norm).or_else(|| cfg.app_mutes.get(k)).copied().unwrap_or(false);
                        cfg.apps.insert(norm, AppRuleV3 {
                            primary_device: v.clone(),
                            secondary_device: sec_dev,
                            secondary_enabled: sec_en,
                            volume: vol,
                            muted,
                        });
                    }
                }
                cfg.sanitize();
                return cfg;
            }
        }
    }

    // Migration từ rules.json cũ nếu có
    let legacy_rules = load_rules();
    if !legacy_rules.is_empty() {
        let mut cfg = ConfigV3::default();
        for (k, v) in &legacy_rules {
            let norm = normalize_app_key(k);
            cfg.apps.insert(norm.clone(), AppRuleV3 {
                primary_device: v.clone(),
                secondary_device: String::new(),
                secondary_enabled: false,
                volume: 1.0,
                muted: false,
            });
            cfg.app_rules.insert(norm, v.clone());
        }
        cfg.sanitize();
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
    fn test_app_rule_v3_default() {
        let rule = AppRuleV3::default();
        assert_eq!(rule.primary_device, "");
        assert_eq!(rule.secondary_device, "");
        assert!(!rule.secondary_enabled);
        assert_eq!(rule.volume, 1.0);
        assert!(!rule.muted);
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

        cfg.set_app_primary_device("spotify.exe".into(), "multi".into());
        cfg.set_app_secondary_device("spotify.exe".into(), "dev_2".into());
        cfg.set_app_secondary_enabled("spotify.exe".into(), true);
        cfg.set_app_volume("spotify.exe".into(), 0.85);
        cfg.set_app_mute("spotify.exe".into(), true);

        let rule = cfg.get_app_rule("Spotify");
        assert_eq!(rule.primary_device, "multi");
        assert_eq!(rule.secondary_device, "dev_2");
        assert!(rule.secondary_enabled);
        assert_eq!(rule.volume, 0.85);
        assert!(rule.muted);

        let rule_exe = cfg.get_app_rule("spotify.exe");
        assert_eq!(rule_exe, rule);

        assert_eq!(cfg.get_app_volume("spotify.exe"), 0.85);
        assert_eq!(cfg.get_app_volume("Spotify"), 0.85);
        assert!(cfg.is_app_muted("spotify.exe"));
        assert!(cfg.is_app_muted("Spotify"));

        // Per-App Dual Output helpers
        cfg.set_app_primary_device("brave.exe".into(), "{dev-speakers}".into());
        assert_eq!(cfg.get_app_primary_device("brave"), Some("{dev-speakers}".to_string()));

        cfg.set_app_secondary_device("brave.exe".into(), "{dev-headphones}".into());
        assert_eq!(cfg.get_app_secondary_device("brave"), Some("{dev-headphones}".to_string()));
        assert_eq!(cfg.get_app_secondary_device("Brave.exe"), Some("{dev-headphones}".to_string()));

        assert!(!cfg.is_app_secondary_enabled("chrome"));
        cfg.set_app_secondary_enabled("brave.exe".into(), true);
        assert!(cfg.is_app_secondary_enabled("brave"));
        assert!(cfg.is_app_secondary_enabled("Brave.exe"));
    }

    #[test]
    fn test_config_v3_roundtrip() {
        let mut cfg = ConfigV3::new();
        cfg.enabled_devices.push("dev_1".into());
        cfg.enabled_devices.push("dev_2".into());
        cfg.device_volumes.insert("dev_1".into(), 0.5);
        cfg.device_volumes.insert("dev_2".into(), 0.8);
        cfg.device_mutes.insert("dev_2".into(), true);
        cfg.set_app_primary_device("chrome.exe".into(), "multi".into());
        cfg.set_app_secondary_device("chrome.exe".into(), "dev_2".into());
        cfg.set_app_secondary_enabled("chrome.exe".into(), true);
        cfg.set_app_primary_device("game.exe".into(), "dev_1".into());

        let json = serde_json::to_string_pretty(&cfg).unwrap();
        let loaded: ConfigV3 = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded, cfg);
        assert_eq!(loaded.get_app_rule("chrome").primary_device, "multi");
        assert_eq!(loaded.get_app_rule("chrome").secondary_device, "dev_2");
        assert!(loaded.get_app_rule("chrome").secondary_enabled);
    }

    #[test]
    fn test_config_v3_legacy_fallback() {
        let legacy_json = r#"{
            "enabled_devices": ["dev_1"],
            "device_volumes": {"dev_1": 0.9},
            "device_mutes": {"dev_1": false},
            "app_rules": {
                "spotify.exe": "multi",
                "discord.exe": "dev_1"
            },
            "app_volumes": {
                "spotify.exe": 0.7
            },
            "app_mutes": {
                "spotify.exe": true
            }
        }"#;

        let cfg: ConfigV3 = serde_json::from_str(legacy_json).unwrap();
        assert!(cfg.apps.is_empty(), "Legacy JSON has no apps field");

        // get_app_rule should fallback to legacy app_rules, app_volumes, app_mutes
        let spotify_rule = cfg.get_app_rule("spotify");
        assert_eq!(spotify_rule.primary_device, "multi");
        assert_eq!(spotify_rule.secondary_device, "");
        assert!(!spotify_rule.secondary_enabled);
        assert_eq!(spotify_rule.volume, 0.7);
        assert!(spotify_rule.muted);

        let discord_rule = cfg.get_app_rule("Discord.exe");
        assert_eq!(discord_rule.primary_device, "dev_1");
        assert_eq!(discord_rule.volume, 1.0);
        assert!(!discord_rule.muted);

        // Unknown app should return default
        let unknown = cfg.get_app_rule("unknown_app.exe");
        assert_eq!(unknown, AppRuleV3::default());
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

    #[test]
    fn test_config_v3_sanitize_and_duplicate_prevention() {
        let mut cfg = ConfigV3::new();
        // 1. Cannot enable secondary if secondary device is empty
        cfg.set_app_primary_device("test.exe".into(), "dev_1".into());
        cfg.set_app_secondary_enabled("test.exe".into(), true);
        assert!(!cfg.get_app_rule("test.exe").secondary_enabled);

        // 2. Cannot set secondary device to same as primary
        cfg.set_app_secondary_device("test.exe".into(), "dev_1".into());
        assert_eq!(cfg.get_app_rule("test.exe").secondary_device, "");
        assert!(!cfg.get_app_rule("test.exe").secondary_enabled);

        // 3. Setting valid secondary device and enabling
        cfg.set_app_secondary_device("test.exe".into(), "dev_2".into());
        cfg.set_app_secondary_enabled("test.exe".into(), true);
        assert_eq!(cfg.get_app_rule("test.exe").secondary_device, "dev_2");
        assert!(cfg.get_app_rule("test.exe").secondary_enabled);

        // 4. Changing primary to match secondary automatically cleans secondary
        cfg.set_app_primary_device("test.exe".into(), "dev_2".into());
        assert_eq!(cfg.get_app_rule("test.exe").primary_device, "dev_2");
        assert_eq!(cfg.get_app_rule("test.exe").secondary_device, "");
        assert!(!cfg.get_app_rule("test.exe").secondary_enabled);

        // 5. sanitize() cleans raw corrupted entries (e.g. from disk or bad external JSON)
        cfg.apps.insert("bad.exe".into(), AppRuleV3 {
            primary_device: "dev_dup".into(),
            secondary_device: "dev_dup".into(),
            secondary_enabled: true,
            volume: 1.0,
            muted: false,
        });
        cfg.sanitize();
        assert_eq!(cfg.get_app_rule("bad.exe").secondary_device, "");
        assert!(!cfg.get_app_rule("bad.exe").secondary_enabled);
    }
}
