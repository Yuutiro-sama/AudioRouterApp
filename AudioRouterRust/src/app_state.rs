//! Global Application State Management.
//!
//! Maintains COM interfaces, device cache, routing rules, MultiOutputEngine, and debounce state.

use std::collections::BTreeMap;
use std::sync::{Arc, RwLock};
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::Audio::{
    eRender, IMMDeviceEnumerator, IMMNotificationClient, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::audio::enumerator::{
    enumerate_render_devices, get_active_app_sessions, get_default_render_device,
    get_pids_by_process_name, AudioDevice,
};
use crate::audio::notification::{
    register_device_hotplug, unregister_device_hotplug, DeviceHotplugClient,
};
use crate::audio::policy_config::{
    get_policy_config_factory, set_app_output_device, IAudioPolicyConfigFactory,
};
use crate::audio::MultiOutputEngine;
use crate::config::rules::{load_config_v3, load_rules, normalize_app_key, save_rules};
use crate::ui::popup::{AppAudioItem, DeviceItem};

/// Central state holder for AudioRouter.
pub struct AppState {
    /// Undocumented WinRT COM factory for per-app routing
    pub factory: IAudioPolicyConfigFactory,
    /// Keep-alive enumerator reference to preserve hardware notification callbacks
    pub enumerator: IMMDeviceEnumerator,
    /// Keep-alive notification client receiving hotplug events
    pub hotplug_client: IMMNotificationClient,
    /// Persisted routing rules: App executable name -> Output Device Endpoint ID
    pub rules: BTreeMap<String, String>,
    /// Cached list of active render audio endpoints
    pub devices: Vec<AudioDevice>,
    /// Flag indicating whether the 150ms hotplug debounce timer is currently running
    pub debounce_timer_active: bool,
    /// Hidden dispatcher HWND for posting async messages
    pub notify_hwnd: HWND,
    /// Multi-Output DSP Mixing Engine coordinator
    pub multi_engine: Arc<RwLock<MultiOutputEngine>>,
}

impl AppState {
    /// Initializes application state, COM singletons, registers hardware callbacks, and launches MultiOutputEngine.
    pub fn new(notify_hwnd: HWND) -> windows::core::Result<Self> {
        // 1. Activate AudioPolicyConfig WinRT factory
        let factory = get_policy_config_factory()?;

        // 2. Instantiate IMMDeviceEnumerator for audio endpoint enumeration and notifications
        let enumerator: IMMDeviceEnumerator = unsafe {
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?
        };

        // 3. Create and register IMMNotificationClient
        let hotplug_client: IMMNotificationClient = DeviceHotplugClient { notify_hwnd }.into();
        register_device_hotplug(&enumerator, &hotplug_client)?;

        // 4. Load persisted routing rules from AppData
        let rules = load_rules();

        // 5. Query active render devices
        let devices = enumerate_render_devices().unwrap_or_default();

        // 6. Initialize Multi-Output Engine with saved ConfigV3
        let mut multi_engine = MultiOutputEngine::new();
        let _ = multi_engine.refresh_targets();

        let config_v3 = load_config_v3();
        let default_dev = get_default_render_device().ok().flatten();
        let default_id = default_dev.as_ref().map(|d| d.id.as_str());

        // If config_v3 has enabled_devices, apply them; if empty, enable default render device
        if config_v3.enabled_devices.is_empty() {
            if let Some(def_id) = default_id {
                multi_engine.set_device_enabled(def_id, true);
            }
        } else {
            for dev_id in &config_v3.enabled_devices {
                multi_engine.set_device_enabled(dev_id, true);
            }
        }
        for (dev_id, &vol) in &config_v3.device_volumes {
            multi_engine.set_device_volume(dev_id, vol);
        }
        for (dev_id, &mute) in &config_v3.device_mutes {
            multi_engine.set_device_muted(dev_id, mute);
        }

        // Start capture and render workers
        let _ = multi_engine.start();

        let multi_engine = Arc::new(RwLock::new(multi_engine));

        Ok(Self {
            factory,
            enumerator,
            hotplug_client,
            rules,
            devices,
            debounce_timer_active: false,
            notify_hwnd,
            multi_engine,
        })
    }

    /// Refreshes the cached list of audio output devices and multi_engine targets.
    pub fn refresh_devices(&mut self) {
        if let Ok(devs) = enumerate_render_devices() {
            self.devices = devs;
        }
        if let Ok(mut engine) = self.multi_engine.write() {
            let _ = engine.refresh_targets();
        }
    }

    /// Synchronizes MultiOutputEngine targets and volume/mute settings with ConfigV3.
    pub fn sync_multi_engine(&self) {
        let config = load_config_v3();
        if let Ok(mut engine) = self.multi_engine.write() {
            let _ = engine.refresh_targets();
            let targets = engine.get_targets();
            for t in targets {
                let enabled = if config.enabled_devices.is_empty() {
                    t.is_default
                } else {
                    config.is_device_enabled(&t.device_id)
                };
                engine.set_device_enabled(&t.device_id, enabled);
                engine.set_device_volume(&t.device_id, config.get_device_volume(&t.device_id));
                engine.set_device_muted(&t.device_id, config.is_device_muted(&t.device_id));
            }
        }
    }

    /// Returns DeviceItem list with live latency, drift, volume, and enabled states.
    pub fn get_device_items(&self) -> Vec<DeviceItem> {
        let config = load_config_v3();
        let engine_guard = self.multi_engine.read().ok();
        let default_dev = get_default_render_device().ok().flatten();
        let default_id = default_dev.as_ref().map(|d| d.id.as_str());

        self.devices
            .iter()
            .map(|dev| {
                let mut item = DeviceItem::from(dev.clone());
                let is_default = default_id == Some(dev.id.as_str());
                item.is_default = is_default;

                if config.enabled_devices.is_empty() && is_default {
                    item.is_enabled = true;
                } else {
                    item.is_enabled = config.is_device_enabled(&dev.id);
                }

                if let Some((vol, mute)) = crate::audio::get_device_endpoint_volume(&dev.id) {
                    item.volume = vol;
                    item.is_muted = mute;
                } else {
                    item.volume = config.get_device_volume(&dev.id);
                    item.is_muted = config.is_device_muted(&dev.id);
                }

                if let Some(ref engine) = engine_guard {
                    if let Some(target) = engine.get_target_by_id(&dev.id) {
                        item.latency_ms = target.get_latency_ms();
                        item.drift_ppm = target.get_drift_ppm() as f32;
                    }
                }
                item
            })
            .collect()
    }

    /// Returns AppAudioItem list with live volume, mute, and routing rules applied.
    pub fn get_app_items(&self) -> Vec<AppAudioItem> {
        let config = load_config_v3();
        let mut apps: Vec<AppAudioItem> = get_active_app_sessions(&self.devices)
            .into_iter()
            .map(Into::into)
            .collect();

        for app in &mut apps {
            if let Some(rule) = config.get_app_rule(&app.name) {
                if rule == "multi" {
                    app.current_device_id = "multi".to_string();
                } else if !rule.is_empty() {
                    app.current_device_id = rule.clone();
                }
            }
            if config.is_app_muted(&app.name) {
                app.is_muted = true;
            }
        }
        apps
    }

    /// Reloads rules from `%AppData%\AudioRouter\rules.json`.
    pub fn reload_rules(&mut self) {
        self.rules = load_rules();
    }

    /// Atomically saves current rules to disk.
    pub fn save_current_rules(&self) -> std::io::Result<()> {
        save_rules(&self.rules)
    }

    /// Case-insensitive, extension-tolerant rule lookup.
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

    /// Iterates through all active application audio sessions and applies persisted rules.
    pub fn apply_all_rules(&mut self, force: bool) -> usize {
        let mut count = 0;
        let config = load_config_v3();
        let sessions = get_active_app_sessions(&self.devices);

        for session in &sessions {
            // Check ConfigV3 app_rules first, fallback to self.rules
            let target_rule = config.get_app_rule(&session.name).cloned().or_else(|| {
                self.get_rule(&session.name).cloned()
            });

            if let Some(rule) = target_rule {
                if rule == "multi" || rule.is_empty() {
                    // Reset to default device so master loopback captures it for multi-output
                    let _ = set_app_output_device(&self.factory, session.pid, eRender, None);
                    let sibling_pids = get_pids_by_process_name(&session.name);
                    for pid in sibling_pids {
                        if pid != session.pid {
                            let _ = set_app_output_device(&self.factory, pid, eRender, None);
                        }
                    }
                    count += 1;
                } else if force || session.current_device_id != rule {
                    let res = set_app_output_device(
                        &self.factory,
                        session.pid,
                        eRender,
                        Some(&rule),
                    );
                    if res.is_ok() {
                        count += 1;
                    }
                    let sibling_pids = get_pids_by_process_name(&session.name);
                    for pid in sibling_pids {
                        if pid != session.pid {
                            let _ = set_app_output_device(
                                &self.factory,
                                pid,
                                eRender,
                                Some(&rule),
                            );
                        }
                    }
                }
            }

            // Restore app volume & mute if specified in ConfigV3
            if let Some(&vol) = config.app_volumes.get(&normalize_app_key(&session.name)) {
                let _ = crate::audio::set_app_volume(session.pid, &session.current_device_id, vol);
            }
            if let Some(&muted) = config.app_mutes.get(&normalize_app_key(&session.name)) {
                let _ = crate::audio::set_app_mute(session.pid, &session.current_device_id, muted);
            }
        }

        count
    }

    /// Applies routing rule for a specific process ID if a rule matches its executable name.
    pub fn apply_rule_for_pid(&self, pid: u32) -> bool {
        let config = load_config_v3();
        let sessions = get_active_app_sessions(&self.devices);

        for session in sessions {
            if session.pid == pid {
                let target_rule = config.get_app_rule(&session.name).cloned().or_else(|| {
                    self.get_rule(&session.name).cloned()
                });

                if let Some(rule) = target_rule {
                    let endpoint_param = if rule == "multi" || rule.is_empty() {
                        None
                    } else {
                        Some(rule.as_str())
                    };

                    let res = set_app_output_device(
                        &self.factory,
                        session.pid,
                        eRender,
                        endpoint_param,
                    );
                    let sibling_pids = get_pids_by_process_name(&session.name);
                    for s_pid in sibling_pids {
                        if s_pid != session.pid {
                            let _ = set_app_output_device(
                                &self.factory,
                                s_pid,
                                eRender,
                                endpoint_param,
                            );
                        }
                    }

                    if let Some(&vol) = config.app_volumes.get(&normalize_app_key(&session.name)) {
                        let _ = crate::audio::set_app_volume(session.pid, &session.current_device_id, vol);
                    }
                    if let Some(&muted) = config.app_mutes.get(&normalize_app_key(&session.name)) {
                        let _ = crate::audio::set_app_mute(session.pid, &session.current_device_id, muted);
                    }

                    return res.is_ok();
                }
                break;
            }
        }

        false
    }

    /// Assigns a routing rule for an application, persists to disk, and applies immediately.
    pub fn set_rule(&mut self, app_name: String, device_id: String) -> windows::core::Result<()> {
        let app_lower = app_name.to_lowercase();
        self.rules.insert(app_lower, device_id);
        let _ = self.save_current_rules();
        self.apply_all_rules(true);
        Ok(())
    }

    /// Removes an application's routing rule, persists to disk, and resets its endpoint to default.
    pub fn remove_rule(&mut self, app_name: &str) -> windows::core::Result<()> {
        let app_lower = app_name.to_lowercase();
        if self.rules.remove(&app_lower).is_some() {
            let _ = self.save_current_rules();
            let sessions = get_active_app_sessions(&self.devices);
            for session in sessions {
                if session.name.to_lowercase() == app_lower {
                    let _ = set_app_output_device(&self.factory, session.pid, eRender, None);
                }
            }
        }
        Ok(())
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        let _ = unregister_device_hotplug(&self.enumerator, &self.hotplug_client);
    }
}
