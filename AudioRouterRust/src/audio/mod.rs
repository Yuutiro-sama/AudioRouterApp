//! Core Audio subsystem for AudioRouterApp.
//!
//! Provides Win32/WASAPI COM-based device enumeration, audio session management,
//! hardware hotplug notifications, and per-app routing via `IAudioPolicyConfigFactory`.

pub mod enumerator;
pub mod notification;
pub mod policy_config;

pub use enumerator::{
    enumerate_capture_devices, enumerate_render_devices, get_active_app_sessions,
    get_default_render_device, set_app_mute, set_app_volume, AppAudioSession, AudioDevice,
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
