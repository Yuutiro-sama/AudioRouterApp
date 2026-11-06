//! Global Application State Management.
//!
//! Maintains COM interfaces, device cache, routing rules, and debounce state.

use std::collections::BTreeMap;
use windows::Win32::Foundation::HWND;
use windows::Win32::Media::Audio::{
    eRender, IMMDeviceEnumerator, IMMNotificationClient, MMDeviceEnumerator,
};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_ALL};

use crate::audio::enumerator::{
    enumerate_render_devices, get_active_app_sessions, get_pids_by_process_name, AudioDevice,
};
use crate::audio::notification::{
    register_device_hotplug, unregister_device_hotplug, DeviceHotplugClient,
};
use crate::audio::policy_config::{
    get_policy_config_factory, set_app_output_device, IAudioPolicyConfigFactory,
};
use crate::config::rules::{load_rules, save_rules};

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
}

impl AppState {
    /// Initializes application state, COM singletons, and registers hardware callbacks.
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

        Ok(Self {
            factory,
            enumerator,
            hotplug_client,
            rules,
            devices,
            debounce_timer_active: false,
            notify_hwnd,
        })
    }

    /// Refreshes the cached list of audio output devices.
    pub fn refresh_devices(&mut self) {
        if let Ok(devs) = enumerate_render_devices() {
            self.devices = devs;
        }
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
    ///
    /// If `force` is true, re-applies routing even if the session currently matches the rule.
    /// Returns the number of application audio endpoints successfully routed.
    pub fn apply_all_rules(&mut self, force: bool) -> usize {
        let mut count = 0;
        let sessions = get_active_app_sessions(&self.devices);

        for session in &sessions {
            if let Some(target_dev_id) = self.get_rule(&session.name).cloned() {
                if force || session.current_device_id != target_dev_id {
                    let res = set_app_output_device(
                        &self.factory,
                        session.pid,
                        eRender,
                        Some(&target_dev_id),
                    );
                    if res.is_ok() {
                        count += 1;
                    }
                    // Also route any other PIDs belonging to this application
                    let sibling_pids = get_pids_by_process_name(&session.name);
                    for pid in sibling_pids {
                        if pid != session.pid {
                            let _ = set_app_output_device(
                                &self.factory,
                                pid,
                                eRender,
                                Some(&target_dev_id),
                            );
                        }
                    }
                }
            }
        }

        count
    }

    /// Applies routing rule for a specific process ID if a rule matches its executable name.
    pub fn apply_rule_for_pid(&self, pid: u32) -> bool {
        let sessions = get_active_app_sessions(&self.devices);

        for session in sessions {
            if session.pid == pid {
                if let Some(target_dev_id) = self.get_rule(&session.name) {
                    let res = set_app_output_device(
                        &self.factory,
                        session.pid,
                        eRender,
                        Some(target_dev_id),
                    );
                    let sibling_pids = get_pids_by_process_name(&session.name);
                    for s_pid in sibling_pids {
                        if s_pid != session.pid {
                            let _ = set_app_output_device(
                                &self.factory,
                                s_pid,
                                eRender,
                                Some(target_dev_id),
                            );
                        }
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
