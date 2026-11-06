//! Audio Policy Config Factory COM Implementation for Windows 10 21H2+ and Windows 11.
//!
//! Provides per-application audio endpoint routing via undocumented WinRT internal API
//! `Windows.Media.Internal.AudioPolicyConfig`.

use core::ffi::c_void;
use windows::core::{GUID, HRESULT, HSTRING, IInspectable, IInspectable_Vtbl, Interface};
use windows::Win32::Media::Audio::{
    eCommunications, eConsole, eMultimedia, eRender, EDataFlow, ERole,
};
use windows::Win32::System::WinRT::RoGetActivationFactory;

/// IID for Windows 10 21H2+ and Windows 11 (IAudioPolicyConfigFactoryVariantFor21H2)
pub const IID_AUDIO_POLICY_CONFIG_21H2: GUID =
    GUID::from_u128(0xab3d4648_e242_459f_b02f_541c70306324);

/// Legacy IID for builds prior to 21H2
pub const IID_AUDIO_POLICY_CONFIG_LEGACY: GUID =
    GUID::from_u128(0x2a59116d_6c4f_45e0_a74f_707e3fef9258);

/// Render interface GUID suffix
pub const DEVINTERFACE_AUDIO_RENDER_SUFFIX: &str = "{e6327cad-dcec-4949-ae8a-991e976a79d2}";

/// Capture interface GUID suffix
pub const DEVINTERFACE_AUDIO_CAPTURE_SUFFIX: &str = "{2eef81be-33fa-4800-9670-1cd474972c3f}";

/// Device Interface Path prefix
pub const DEVINTERFACE_PATH_PREFIX: &str = r"\\?\SWD#MMDEVAPI#";

#[repr(transparent)]
#[derive(Clone, PartialEq, Eq)]
pub struct IAudioPolicyConfigFactory(pub IInspectable);

unsafe impl Interface for IAudioPolicyConfigFactory {
    type Vtable = IAudioPolicyConfigFactory_Vtbl;
    const IID: GUID = IID_AUDIO_POLICY_CONFIG_21H2;
}

impl core::ops::Deref for IAudioPolicyConfigFactory {
    type Target = IInspectable;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

#[repr(C)]
#[allow(non_snake_case)]
pub struct IAudioPolicyConfigFactory_Vtbl {
    pub base__: IInspectable_Vtbl, // Slots 0 -> 5 (3 IUnknown + 3 IInspectable)
    // 25 unpublished internal methods of the OS (Slots 6 -> 30)
    pub padding_slots: [usize; 25],
    // 19 Volume Group & Chat Context methods (Slots 31 -> 49)
    pub add_ctx_volume_changed: usize,
    pub remove_ctx_volume_changed: usize,
    pub add_ringer_vibrate_state_changed: usize,
    pub remove_ringer_vibrate_state_changed: usize,
    pub set_volume_group_gain_for_id: usize,
    pub get_volume_group_gain_for_id: usize,
    pub get_active_volume_group_for_endpoint_id: usize,
    pub get_volume_groups_for_endpoint: usize,
    pub get_current_volume_context: usize,
    pub set_volume_group_mute_for_id: usize,
    pub get_volume_group_mute_for_id: usize,
    pub set_ringer_vibrate_state: usize,
    pub get_ringer_vibrate_state: usize,
    pub set_preferred_chat_application: usize,
    pub reset_preferred_chat_application: usize,
    pub get_preferred_chat_application: usize,
    pub get_current_chat_applications: usize,
    pub add_chat_context_changed: usize,
    pub remove_chat_context_changed: usize,
    // 3 core routing methods (Slots 50, 51, 52)
    pub SetPersistedDefaultAudioEndpoint: unsafe extern "system" fn(
        this: *mut c_void,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut c_void, // HSTRING ABI pointer
    ) -> HRESULT,
    pub GetPersistedDefaultAudioEndpoint: unsafe extern "system" fn(
        this: *mut c_void,
        process_id: u32,
        flow: EDataFlow,
        role: ERole,
        device_id: *mut *mut c_void, // Out HSTRING ABI pointer
    ) -> HRESULT,
    pub ClearAllPersistedApplicationDefaultEndpoints: unsafe extern "system" fn(
        this: *mut c_void,
    ) -> HRESULT,
}

/// Retrieves an instance of `IAudioPolicyConfigFactory` using `RoGetActivationFactory`.
pub fn get_policy_config_factory() -> windows::core::Result<IAudioPolicyConfigFactory> {
    let class_name = HSTRING::from("Windows.Media.Internal.AudioPolicyConfig");
    unsafe { RoGetActivationFactory(&class_name) }
}

/// Normalizes a short endpoint ID (e.g. `{0.0.0.00000000}.{guid}`) to a full Device Interface Path.
/// If `short_device_id` already starts with `\\?\`, it is preserved.
pub fn format_endpoint_interface_path(short_device_id: &str, flow: EDataFlow) -> String {
    if short_device_id.starts_with(DEVINTERFACE_PATH_PREFIX) {
        short_device_id.to_string()
    } else {
        let suffix = if flow == eRender {
            DEVINTERFACE_AUDIO_RENDER_SUFFIX
        } else {
            DEVINTERFACE_AUDIO_CAPTURE_SUFFIX
        };
        format!(r"{}{}#{}", DEVINTERFACE_PATH_PREFIX, short_device_id, suffix)
    }
}

/// Extracts the short device ID from a full Device Interface Path.
pub fn normalize_device_id_to_endpoint(device_interface: &str) -> &str {
    let s = device_interface
        .strip_prefix(DEVINTERFACE_PATH_PREFIX)
        .unwrap_or(device_interface);
    if let Some(idx) = s.find("#{") {
        &s[..idx]
    } else {
        s
    }
}

/// Sets the audio output/input routing device for a specific process ID.
///
/// - If `short_device_id` is `Some(id)` and non-empty, wraps with the appropriate DEVINTERFACE path.
/// - If `short_device_id` is `None` or empty, resets the process to the system default endpoint.
/// - Applies synchronously to all three audio roles (`eConsole`, `eMultimedia`, `eCommunications`).
pub fn set_app_output_device(
    factory: &IAudioPolicyConfigFactory,
    pid: u32,
    flow: EDataFlow,
    short_device_id: Option<&str>,
) -> windows::core::Result<()> {
    let formatted_hstring = match short_device_id {
        Some(id) if !id.trim().is_empty() => {
            let full_path = format_endpoint_interface_path(id.trim(), flow);
            HSTRING::from(full_path)
        }
        _ => HSTRING::new(),
    };

    let roles = [eConsole, eMultimedia, eCommunications];
    for role in roles {
        unsafe {
            let hstring_raw: *mut c_void = core::mem::transmute_copy(&formatted_hstring);
            (factory.vtable().SetPersistedDefaultAudioEndpoint)(
                factory.as_raw(),
                pid,
                flow,
                role,
                hstring_raw,
            )
            .ok()?;
        }
    }
    Ok(())
}

/// Retrieves the current persisted default endpoint ID for a specific process ID and role.
pub fn get_app_output_device(
    factory: &IAudioPolicyConfigFactory,
    pid: u32,
    flow: EDataFlow,
    role: ERole,
) -> windows::core::Result<Option<String>> {
    let mut raw_hstring: *mut c_void = core::ptr::null_mut();
    unsafe {
        let hr = (factory.vtable().GetPersistedDefaultAudioEndpoint)(
            factory.as_raw(),
            pid,
            flow,
            role,
            &mut raw_hstring,
        );
        if hr.is_err() || raw_hstring.is_null() {
            return Ok(None);
        }
        let hstring: HSTRING = core::mem::transmute(raw_hstring);
        let s = hstring.to_string();
        if s.trim().is_empty() {
            Ok(None)
        } else {
            let short_id = normalize_device_id_to_endpoint(&s).to_string();
            Ok(Some(short_id))
        }
    }
}

/// Clears all persisted application default endpoints across the entire system.
pub fn clear_all_persisted_application_default_endpoints(
    factory: &IAudioPolicyConfigFactory,
) -> windows::core::Result<()> {
    unsafe {
        (factory.vtable().ClearAllPersistedApplicationDefaultEndpoints)(factory.as_raw()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_and_normalize_endpoint_interface_path() {
        let short_id = "{0.0.0.00000000}.{a1b2c3d4-e5f6-7890-abcd-ef1234567890}";
        let formatted = format_endpoint_interface_path(short_id, eRender);
        assert!(formatted.starts_with(DEVINTERFACE_PATH_PREFIX));
        assert!(formatted.ends_with(DEVINTERFACE_AUDIO_RENDER_SUFFIX));

        let normalized = normalize_device_id_to_endpoint(&formatted);
        assert_eq!(normalized, short_id);
    }

    #[test]
    fn test_format_endpoint_already_prefixed() {
        let full_path = r"\\?\SWD#MMDEVAPI#{0.0.0.00000000}.{123}#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
        let formatted = format_endpoint_interface_path(full_path, eRender);
        assert_eq!(formatted, full_path);
    }

    #[test]
    fn test_vtable_layout_size() {
        use std::mem::size_of;
        // base__: 6 pointers (3 IUnknown + 3 IInspectable)
        // padding: 25 pointers
        // volume/chat: 19 pointers
        // routing: 3 pointers
        // Total slots: 6 + 25 + 19 + 3 = 53 function pointers
        assert_eq!(size_of::<IAudioPolicyConfigFactory_Vtbl>(), 53 * size_of::<usize>());
    }
}
