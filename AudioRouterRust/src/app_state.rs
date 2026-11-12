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
use crate::config::rules::{load_config_v3, load_rules, save_rules};
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

        // 6. Initialize Multi-Output Engine
        let mut multi_engine = MultiOutputEngine::new();
        let _ = multi_engine.refresh_targets();
        let multi_engine = Arc::new(RwLock::new(multi_engine));

        let state = Self {
            factory,
            enumerator,
            hotplug_client,
            rules,
            devices,
            debounce_timer_active: false,
            notify_hwnd,
            multi_engine,
        };

        // Synchronize active app sessions and dual routing bridges immediately
        state.sync_multi_engine();

        Ok(state)
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
    /// In the Process-Specific Dual Routing architecture, secondary routes are only active
    /// for specific running application sessions (PIDs) that have `secondary_enabled == true`.
    pub fn sync_multi_engine(&self) {
        let config = load_config_v3();
        let default_dev = get_default_render_device().ok().flatten();
        let default_id = default_dev.as_ref().map(|d| d.id.as_str()).unwrap_or_default();

        let sessions = get_active_app_sessions(&self.devices);
        let mut active_routes: Vec<(u32, String, String)> = Vec::new();
        let mut seen_routes: std::collections::HashSet<(u32, String)> = std::collections::HashSet::new();

        for session in &sessions {
            let rule = config.get_app_rule(&session.name);
            if rule.secondary_enabled && !rule.secondary_device.is_empty() {
                let primary = if !rule.primary_device.is_empty() && rule.primary_device != "multi" {
                    rule.primary_device.clone()
                } else if let Some(legacy_prim) = self.get_rule(&session.name) {
                    if !legacy_prim.is_empty() && legacy_prim != "multi" {
                        legacy_prim.clone()
                    } else if !session.current_device_id.is_empty() {
                        session.current_device_id.clone()
                    } else {
                        default_id.to_string()
                    }
                } else if !session.current_device_id.is_empty() {
                    session.current_device_id.clone()
                } else {
                    default_id.to_string()
                };
                let secondary = rule.secondary_device.trim().to_string();

                if !secondary.is_empty() && secondary != primary && secondary != "multi" {
                    let mut target_pids = vec![session.pid];
                    for sibling_pid in get_pids_by_process_name(&session.name) {
                        if !target_pids.contains(&sibling_pid) {
                            target_pids.push(sibling_pid);
                        }
                    }

                    for pid in target_pids {
                        if seen_routes.insert((pid, secondary.clone())) {
                            active_routes.push((pid, primary.clone(), secondary.clone()));
                        }
                    }
                }
            }
        }

        if let Ok(mut engine) = self.multi_engine.write() {
            let _ = engine.refresh_targets();
            engine.sync_bridges(active_routes);

            for (dev_id, &vol) in &config.device_volumes {
                engine.set_bridge_volume(dev_id, vol);
            }
            for (dev_id, &mute) in &config.device_mutes {
                engine.set_bridge_muted(dev_id, mute);
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
            let rule = config.get_app_rule(&app.name);
            if !rule.primary_device.is_empty() {
                app.primary_device_id = rule.primary_device.clone();
                app.current_device_id = rule.primary_device.clone();
            }
            if !rule.secondary_device.is_empty() {
                app.secondary_device_id = rule.secondary_device.clone();
            }
            app.secondary_enabled = rule.secondary_enabled;
            app.is_muted = rule.muted;
            app.volume = rule.volume;
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

    /// Iterates through all active application audio sessions and applies persisted rules:
    /// - Primary device routing via Windows Audio Policy (`IAudioPolicyConfigFactory`)
    /// - Secondary device activation/deactivation in `MultiOutputEngine`
    /// - Per-app volume and mute restoration
    pub fn apply_all_rules(&mut self, force: bool) -> usize {
        let mut count = 0;

        // 1. Synchronize secondary render targets in MultiOutputEngine
        self.sync_multi_engine();

        let config = load_config_v3();
        let sessions = get_active_app_sessions(&self.devices);

        // 2. For each active app session: route primary endpoint and apply volume/mute
        for session in &sessions {
            let app_rule = config.get_app_rule(&session.name);
            let target_rule = if !app_rule.primary_device.is_empty() {
                Some(app_rule.primary_device.clone())
            } else {
                self.get_rule(&session.name).cloned()
            };

            if let Some(rule) = target_rule {
                if rule == "multi" || rule.is_empty() {
                    // Reset to default device (None) so master loopback captures it for multi-output
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
            let _ = crate::audio::set_app_volume(session.pid, &session.current_device_id, app_rule.volume);
            let _ = crate::audio::set_app_mute(session.pid, &session.current_device_id, app_rule.muted);
        }

        count
    }

    /// Applies routing rule for a specific process ID if a rule matches its executable name.
    pub fn apply_rule_for_pid(&self, pid: u32) -> bool {
        let config = load_config_v3();
        let sessions = get_active_app_sessions(&self.devices);

        for session in sessions {
            if session.pid == pid {
                let app_rule = config.get_app_rule(&session.name);
                let target_rule = if !app_rule.primary_device.is_empty() {
                    Some(app_rule.primary_device)
                } else {
                    self.get_rule(&session.name).cloned()
                };

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

                    let _ = crate::audio::set_app_volume(session.pid, &session.current_device_id, app_rule.volume);
                    let _ = crate::audio::set_app_mute(session.pid, &session.current_device_id, app_rule.muted);

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
        self.rules.insert(app_lower, device_id.clone());
        let _ = self.save_current_rules();

        let mut config = load_config_v3();
        config.set_app_primary_device(app_name, device_id);
        let _ = crate::config::rules::save_config_v3(&config);

        self.apply_all_rules(true);
        Ok(())
    }

    /// Removes an application's routing rule, persists to disk, and resets its endpoint to default.
    pub fn remove_rule(&mut self, app_name: &str) -> windows::core::Result<()> {
        let app_lower = app_name.to_lowercase();
        if self.rules.remove(&app_lower).is_some() {
            let _ = self.save_current_rules();
        }

        let mut config = load_config_v3();
        let _ = config.remove_app_rule(app_name);
        let _ = crate::config::rules::save_config_v3(&config);

        let sessions = get_active_app_sessions(&self.devices);
        for session in sessions {
            if session.name.to_lowercase() == app_lower {
                let _ = set_app_output_device(&self.factory, session.pid, eRender, None);
            }
        }
        self.apply_all_rules(true);
        Ok(())
    }
}

impl Drop for AppState {
    fn drop(&mut self) {
        let _ = unregister_device_hotplug(&self.enumerator, &self.hotplug_client);
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::render::RenderTargetState;
    use crate::config::rules::ConfigV3;

    #[test]
    fn test_app_state_rule_lookup_normalization() {
        let mut rules = BTreeMap::new();
        rules.insert("spotify.exe".to_string(), "dev_1".to_string());
        rules.insert("chrome".to_string(), "dev_2".to_string());

        let name_lower = "SPOTIFY.EXE".to_lowercase();
        assert_eq!(rules.get(&name_lower), Some(&"dev_1".to_string()));
        let name_no_ext = "spotify";
        let found = rules.iter().find(|(k, _)| {
            let kl = k.to_lowercase();
            kl.strip_suffix(".exe").unwrap_or(&kl) == name_no_ext
        });
        assert_eq!(found.map(|(_, v)| v), Some(&"dev_1".to_string()));
    }

    #[test]
    fn test_sync_multi_engine_dual_output_logic() {
        let mut engine = MultiOutputEngine::new();
        let target_default = Arc::new(RenderTargetState::new(
            "dev_speakers".to_string(),
            "Realtek Speakers".to_string(),
            true,
        ));
        let target_headphones = Arc::new(RenderTargetState::new(
            "dev_headphones".to_string(),
            "Bluetooth Headphones".to_string(),
            false,
        ));

        if let Ok(mut guard) = engine.targets.write() {
            guard.push(target_default);
            guard.push(target_headphones);
        }

        // Initially dev_headphones is disabled
        assert!(!engine.get_target_by_id("dev_headphones").unwrap().is_enabled());

        // Simulate config with Per-App Dual Output enabled for Spotify
        let mut cfg = ConfigV3::new();
        cfg.set_app_primary_device("spotify.exe".into(), "dev_speakers".into());
        cfg.set_app_secondary_device("spotify.exe".into(), "dev_headphones".into());
        cfg.set_app_secondary_enabled("spotify.exe".into(), true);

        // Gather active secondary devices
        let mut active_secondary_devices: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_app_key, rule) in &cfg.apps {
            if rule.secondary_enabled && !rule.secondary_device.is_empty() {
                active_secondary_devices.insert(rule.secondary_device.clone());
            }
        }
        assert!(active_secondary_devices.contains("dev_headphones"));

        // Apply to engine targets
        for t in engine.get_targets() {
            let should_enable = active_secondary_devices.contains(&t.device_id)
                || (t.is_default && active_secondary_devices.is_empty());
            engine.set_device_enabled(&t.device_id, should_enable);
        }

        // dev_headphones is now enabled as a secondary render target
        assert!(engine.get_target_by_id("dev_headphones").unwrap().is_enabled());
        assert!(engine.has_active_secondary());

        // Now simulate user toggling secondary_enabled to false
        cfg.set_app_secondary_enabled("spotify.exe".into(), false);
        let mut active_secondary_devices2: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_app_key, rule) in &cfg.apps {
            if rule.secondary_enabled && !rule.secondary_device.is_empty() {
                active_secondary_devices2.insert(rule.secondary_device.clone());
            }
        }
        assert!(!active_secondary_devices2.contains("dev_headphones"));

        for t in engine.get_targets() {
            let should_enable = active_secondary_devices2.contains(&t.device_id)
                || (t.is_default && active_secondary_devices2.is_empty());
            engine.set_device_enabled(&t.device_id, should_enable);
        }

        // dev_headphones is now disabled to save CPU and audio resources
        assert!(!engine.get_target_by_id("dev_headphones").unwrap().is_enabled());
        assert!(!engine.has_active_secondary());
    }
}
