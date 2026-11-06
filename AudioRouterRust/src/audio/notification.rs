//! Audio Device Hotplug and Session Notification Module.
//!
//! Implements `IMMNotificationClient` and `IAudioSessionNotification` to dispatch
//! asynchronous notifications back to the Win32 message loop via `PostMessageW`.

use windows::core::{implement, Interface, PCWSTR, Result};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::Audio::{
    DEVICE_STATE, EDataFlow, ERole, IAudioSessionControl, IAudioSessionControl2,
    IAudioSessionNotification, IAudioSessionNotification_Impl, IMMDeviceEnumerator,
    IMMNotificationClient, IMMNotificationClient_Impl,
};
use windows::Win32::UI::Shell::PropertiesSystem::PROPERTYKEY;
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::ipc_messages::{WM_APP_DEFAULT_CHANGED, WM_APP_DEVICE_HOTPLUG, WM_APP_SESSION_CREATED};

/// Non-blocking callback client implementing `IMMNotificationClient`.
///
/// Posts lightweight Win32 messages to the dispatcher window so heavy COM/WASAPI
/// operations are never performed on the RPC audio notification thread.
#[implement(IMMNotificationClient)]
pub struct DeviceHotplugClient {
    pub notify_hwnd: HWND,
}

impl IMMNotificationClient_Impl for DeviceHotplugClient_Impl {
    fn OnDeviceStateChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _dwnewstate: DEVICE_STATE,
    ) -> Result<()> {
        unsafe {
            let _ = PostMessageW(
                self.notify_hwnd,
                WM_APP_DEVICE_HOTPLUG,
                WPARAM(0),
                LPARAM(0),
            );
        }
        Ok(())
    }

    fn OnDeviceAdded(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> {
        unsafe {
            let _ = PostMessageW(
                self.notify_hwnd,
                WM_APP_DEVICE_HOTPLUG,
                WPARAM(1),
                LPARAM(0),
            );
        }
        Ok(())
    }

    fn OnDeviceRemoved(&self, _pwstrdeviceid: &PCWSTR) -> Result<()> {
        unsafe {
            let _ = PostMessageW(
                self.notify_hwnd,
                WM_APP_DEVICE_HOTPLUG,
                WPARAM(2),
                LPARAM(0),
            );
        }
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        flow: EDataFlow,
        role: ERole,
        _pwstrdefaultdeviceid: &PCWSTR,
    ) -> Result<()> {
        unsafe {
            let _ = PostMessageW(
                self.notify_hwnd,
                WM_APP_DEFAULT_CHANGED,
                WPARAM(flow.0 as usize),
                LPARAM(role.0 as isize),
            );
        }
        Ok(())
    }

    fn OnPropertyValueChanged(
        &self,
        _pwstrdeviceid: &PCWSTR,
        _key: &PROPERTYKEY,
    ) -> Result<()> {
        Ok(())
    }
}

/// Non-blocking callback client implementing `IAudioSessionNotification`.
///
/// Triggers when a newly launched process creates an audio stream.
#[implement(IAudioSessionNotification)]
pub struct AppSessionNotificationClient {
    pub notify_hwnd: HWND,
}

impl IAudioSessionNotification_Impl for AppSessionNotificationClient_Impl {
    fn OnSessionCreated(&self, newsession: Option<&IAudioSessionControl>) -> Result<()> {
        let mut pid: u32 = 0;
        if let Some(control) = newsession {
            if let Ok(control2) = control.cast::<IAudioSessionControl2>() {
                unsafe {
                    if let Ok(p) = control2.GetProcessId() {
                        pid = p;
                    }
                }
            }
        }
        unsafe {
            let _ = PostMessageW(
                self.notify_hwnd,
                WM_APP_SESSION_CREATED,
                WPARAM(pid as usize),
                LPARAM(0),
            );
        }
        Ok(())
    }
}

/// Registers an `IMMNotificationClient` with the system device enumerator.
pub fn register_device_hotplug(
    enumerator: &IMMDeviceEnumerator,
    client: &IMMNotificationClient,
) -> Result<()> {
    unsafe { enumerator.RegisterEndpointNotificationCallback(client) }
}

/// Unregisters an `IMMNotificationClient` from the system device enumerator.
pub fn unregister_device_hotplug(
    enumerator: &IMMDeviceEnumerator,
    client: &IMMNotificationClient,
) -> Result<()> {
    unsafe { enumerator.UnregisterEndpointNotificationCallback(client) }
}
