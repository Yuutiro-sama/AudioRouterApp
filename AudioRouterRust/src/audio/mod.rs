//! Core Audio subsystem for AudioRouterApp.
//!
//! Provides Win32/WASAPI COM-based device enumeration, audio session management,
//! hardware hotplug notifications, per-app routing via `IAudioPolicyConfigFactory`,
//! and real-time Multi-Output DSP Mixing Engine.

pub mod capture;
pub mod dsp;
pub mod enumerator;
pub mod notification;
pub mod policy_config;
pub mod render;
pub mod ring_buffer;

pub use capture::{
    spawn_master_loopback_worker, AudioClientActivationParams, AudioClientActivationType,
    AudioClientProcessLoopbackParams, ProcessLoopbackMode,
    VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
};
pub use dsp::AdaptiveResamplerDsp;
pub use enumerator::{
    enumerate_capture_devices, enumerate_render_devices, get_active_app_sessions,
    get_default_render_device, get_device_endpoint_volume, get_pids_by_process_name,
    set_app_mute, set_app_volume, set_device_endpoint_mute, set_device_endpoint_volume,
    AppAudioSession, AudioDevice,
};
pub use notification::{
    register_device_hotplug, unregister_device_hotplug, AppSessionNotificationClient,
    DeviceHotplugClient,
};
pub use policy_config::{
    clear_all_persisted_application_default_endpoints, format_endpoint_interface_path,
    get_app_output_device, get_policy_config_factory, normalize_device_id_to_endpoint,
    set_app_output_device, IAudioPolicyConfigFactory, IAudioPolicyConfigFactory_Vtbl,
    DEVINTERFACE_AUDIO_CAPTURE_SUFFIX, DEVINTERFACE_AUDIO_RENDER_SUFFIX,
    DEVINTERFACE_PATH_PREFIX, IID_AUDIO_POLICY_CONFIG_21H2, IID_AUDIO_POLICY_CONFIG_LEGACY,
};
pub use render::{spawn_render_worker, RenderTargetState};
pub use ring_buffer::SpscRingBuffer;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, RwLock};

/// High-performance Multi-Output Audio Engine coordinator.
pub struct MultiOutputEngine {
    pub(crate) targets: Arc<RwLock<Vec<Arc<RenderTargetState>>>>,
    capture_handle: Option<std::thread::JoinHandle<()>>,
    render_workers: HashMap<String, std::thread::JoinHandle<()>>,
    stop_signal: Arc<AtomicBool>,
    capture_sample_rate: Arc<AtomicU32>,
    is_running: bool,
}

impl MultiOutputEngine {
    /// Creates a new MultiOutputEngine instance.
    pub fn new() -> Self {
        Self {
            targets: Arc::new(RwLock::new(Vec::new())),
            capture_handle: None,
            render_workers: HashMap::new(),
            stop_signal: Arc::new(AtomicBool::new(false)),
            capture_sample_rate: Arc::new(AtomicU32::new(48000)),
            is_running: false,
        }
    }

    /// Returns whether the engine is currently running.
    pub fn is_running(&self) -> bool {
        self.is_running
    }

    /// Returns a list of all render targets.
    pub fn get_targets(&self) -> Vec<Arc<RenderTargetState>> {
        self.targets.read().map(|g| g.clone()).unwrap_or_default()
    }

    /// Finds a target by its endpoint device ID.
    pub fn get_target_by_id(&self, device_id: &str) -> Option<Arc<RenderTargetState>> {
        self.targets
            .read()
            .ok()?
            .iter()
            .find(|t| t.device_id == device_id)
            .cloned()
    }

    /// Refreshes the list of available audio endpoints while preserving existing states.
    pub fn refresh_targets(&mut self) -> windows::core::Result<()> {
        let active_devices = enumerate_render_devices()?;
        let default_dev = get_default_render_device().ok().flatten();

        let mut existing_map: HashMap<String, Arc<RenderTargetState>> = HashMap::new();
        if let Ok(guard) = self.targets.read() {
            for t in guard.iter() {
                existing_map.insert(t.device_id.clone(), t.clone());
            }
        }

        let mut new_targets = Vec::new();
        for dev in active_devices {
            let is_default = default_dev
                .as_ref()
                .map(|d| d.id == dev.id)
                .unwrap_or(false);

            if let Some(existing) = existing_map.remove(&dev.id) {
                // Keep existing state, but update default status if changed
                if existing.is_default != is_default {
                    let updated = Arc::new(RenderTargetState::new(
                        dev.id.clone(),
                        dev.name.clone(),
                        is_default,
                    ));
                    updated.set_volume(existing.get_volume());
                    updated.set_muted(existing.is_muted());
                    updated.set_enabled(existing.is_enabled());
                    new_targets.push(updated);
                } else {
                    new_targets.push(existing);
                }
            } else {
                new_targets.push(Arc::new(RenderTargetState::new(
                    dev.id,
                    dev.name,
                    is_default,
                )));
            }
        }

        if let Ok(mut guard) = self.targets.write() {
            *guard = new_targets;
        }

        // If running, ensure active workers are spawned for any newly enabled targets
        if self.is_running {
            self.ensure_render_workers();
        }

        Ok(())
    }

    /// Starts loopback capture and all active render target workers.
    pub fn start(&mut self) -> windows::core::Result<()> {
        if self.is_running {
            return Ok(());
        }

        self.stop_signal.store(false, Ordering::Release);
        self.refresh_targets()?;

        // 1. Spawn master loopback worker
        let capture_handle = spawn_master_loopback_worker(
            self.targets.clone(),
            self.capture_sample_rate.clone(),
            self.stop_signal.clone(),
        );
        self.capture_handle = Some(capture_handle);

        self.is_running = true;

        // 2. Spawn render workers for all currently enabled endpoints
        self.ensure_render_workers();

        Ok(())
    }

    /// Stops loopback capture and all render target workers.
    pub fn stop(&mut self) {
        if !self.is_running {
            return;
        }

        self.stop_signal.store(true, Ordering::Release);

        if let Some(handle) = self.capture_handle.take() {
            let _ = handle.join();
        }

        for (_, handle) in self.render_workers.drain() {
            let _ = handle.join();
        }

        self.is_running = false;
    }

    /// Enables or disables output to a specific audio endpoint.
    pub fn set_device_enabled(&mut self, device_id: &str, enabled: bool) {
        if let Some(target) = self.get_target_by_id(device_id) {
            target.set_enabled(enabled);
            if !enabled {
                // If disabled, stop and join worker thread immediately to release resources
                if let Some(handle) = self.render_workers.remove(device_id) {
                    let _ = handle.join();
                }
            } else if self.is_running {
                // Remove any stale finished handle before re-spawning
                if let Some(handle) = self.render_workers.get(device_id) {
                    if handle.is_finished() {
                        if let Some(h) = self.render_workers.remove(device_id) {
                            let _ = h.join();
                        }
                    }
                }
                self.ensure_render_workers();
            }
        }
    }

    /// Sets the volume scalar [0.0 .. 1.0] for a specific audio endpoint.
    pub fn set_device_volume(&self, device_id: &str, volume: f32) {
        if let Some(target) = self.get_target_by_id(device_id) {
            target.set_volume(volume);
        }
    }

    /// Mutes or unmutes a specific audio endpoint.
    pub fn set_device_muted(&self, device_id: &str, muted: bool) {
        if let Some(target) = self.get_target_by_id(device_id) {
            target.set_muted(muted);
        }
    }

    /// Returns list of device IDs for all currently enabled render targets.
    pub fn get_enabled_device_ids(&self) -> Vec<String> {
        self.get_targets()
            .into_iter()
            .filter(|t| t.is_enabled())
            .map(|t| t.device_id.clone())
            .collect()
    }

    /// Checks if any secondary (non-default) device is currently enabled.
    pub fn has_active_secondary(&self) -> bool {
        self.get_targets()
            .into_iter()
            .any(|t| !t.is_default && t.is_enabled())
    }

    /// Helper to ensure all enabled targets have an active render worker thread running.
    fn ensure_render_workers(&mut self) {
        // Retain only active workers; join any finished ones
        let finished_keys: Vec<String> = self
            .render_workers
            .iter()
            .filter(|(_, handle)| handle.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for key in finished_keys {
            if let Some(handle) = self.render_workers.remove(&key) {
                let _ = handle.join();
            }
        }

        let capture_rate = self.capture_sample_rate.load(Ordering::Acquire);
        let targets = self.get_targets();

        for target in targets {
            if target.is_enabled() && !self.render_workers.contains_key(&target.device_id) {
                let worker_handle = spawn_render_worker(
                    target.clone(),
                    capture_rate,
                    self.stop_signal.clone(),
                );
                self.render_workers.insert(target.device_id.clone(), worker_handle);
            }
        }
    }
}

impl Default for MultiOutputEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for MultiOutputEngine {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_multi_output_engine_lifecycle_and_helpers() {
        let mut engine = MultiOutputEngine::new();
        assert!(!engine.is_running());

        // Add dummy target states
        let target1 = Arc::new(RenderTargetState::new(
            "dev_primary".to_string(),
            "Speakers".to_string(),
            true, // default
        ));
        let target2 = Arc::new(RenderTargetState::new(
            "dev_secondary".to_string(),
            "Headphones".to_string(),
            false, // non-default
        ));

        if let Ok(mut guard) = engine.targets.write() {
            guard.push(target1);
            guard.push(target2);
        }

        assert_eq!(engine.get_targets().len(), 2);

        // Initially dev_primary is enabled (since default), dev_secondary is disabled
        assert!(engine.get_target_by_id("dev_primary").unwrap().is_enabled());
        assert!(!engine.get_target_by_id("dev_secondary").unwrap().is_enabled());
        assert!(!engine.has_active_secondary());

        // Enable secondary device
        engine.set_device_enabled("dev_secondary", true);
        assert!(engine.get_target_by_id("dev_secondary").unwrap().is_enabled());
        assert!(engine.has_active_secondary());

        let enabled_ids = engine.get_enabled_device_ids();
        assert!(enabled_ids.contains(&"dev_primary".to_string()));
        assert!(enabled_ids.contains(&"dev_secondary".to_string()));

        // Set volume and mute on secondary device
        engine.set_device_volume("dev_secondary", 0.65);
        assert_eq!(engine.get_target_by_id("dev_secondary").unwrap().get_volume(), 0.65);

        engine.set_device_muted("dev_secondary", true);
        assert!(engine.get_target_by_id("dev_secondary").unwrap().is_muted());

        // Disable secondary device
        engine.set_device_enabled("dev_secondary", false);
        assert!(!engine.get_target_by_id("dev_secondary").unwrap().is_enabled());
        assert!(!engine.has_active_secondary());
    }
}
