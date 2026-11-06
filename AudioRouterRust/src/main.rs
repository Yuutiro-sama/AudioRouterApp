#![windows_subsystem = "windows"]

//! AudioRouterApp - High-Performance Native Rust Audio Router.
//!
//! Sub-3MB binary footprint and ultra-low RAM usage (< 2.5MB idle).
//! Routes per-application audio streams to designated endpoints using Windows Core Audio & WinRT APIs.

pub mod app_state;
pub mod audio;
pub mod config;
pub mod ipc_messages;
pub mod ui;

use windows::core::w;
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HWND, LPARAM, LRESULT, WPARAM,
};
use windows::Win32::System::Com::{
    CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, PostQuitMessage, RegisterClassExW, SetTimer,
    SetWindowLongPtrW, TranslateMessage, GWLP_USERDATA, MSG, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_POPUP,
    WM_COMMAND, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONUP,
    WM_RBUTTONUP, WM_TIMER,
};

use crate::app_state::AppState;
use crate::audio::enumerator::get_active_app_sessions;
use crate::ipc_messages::{
    DEBOUNCE_DELAY_MS, IDM_APPLY_RULES, IDM_EXIT, IDM_SHOW_UI, IDM_STARTUP,
    TIMER_ID_DEBOUNCE, WM_APP_APPLY_RULES, WM_APP_DEFAULT_CHANGED,
    WM_APP_DEVICE_HOTPLUG, WM_APP_SESSION_CREATED, WM_APP_TRAY,
};
use crate::ui::popup::{close_flyout, toggle_flyout, AppAudioItem, DeviceItem};
use crate::ui::tray::{register_taskbar_created_message, TrayIcon};

/// Context structure stored in the hidden dispatcher window's `GWLP_USERDATA`.
struct DispatcherData {
    app_state: AppState,
    tray_icon: TrayIcon,
    taskbar_created_msg: u32,
}

static LAST_TRAY_CLICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Hidden dispatcher window procedure.
///
/// Processes Shell notification callbacks, hotplug debounce timers, session events,
/// and context menu actions without consuming resources for an active UI.
unsafe extern "system" fn dispatcher_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let data_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut DispatcherData;
    if data_ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let data = &mut *data_ptr;

    // 1. Check if Explorer was restarted (TaskbarCreated broadcast)
    if msg == data.taskbar_created_msg {
        let _ = data.tray_icon.recreate();
        return LRESULT(0);
    }

    match msg {
        WM_APP_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            // Left click or notification select with 250ms debounce
            if event == WM_LBUTTONUP || event == 0x0400 /* NIN_SELECT */ {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last = LAST_TRAY_CLICK.swap(now, std::sync::atomic::Ordering::SeqCst);
                if now.saturating_sub(last) < 250 {
                    return LRESULT(0); // Ignore duplicate shell events within 250ms
                }

                data.app_state.refresh_devices();
                let device_items: Vec<DeviceItem> =
                    data.app_state.devices.iter().cloned().map(Into::into).collect();
                let app_items: Vec<AppAudioItem> =
                    get_active_app_sessions(&data.app_state.devices)
                        .into_iter()
                        .map(Into::into)
                        .collect();
                let tray_rect = data.tray_icon.get_rect();
                toggle_flyout(hwnd, device_items, app_items, tray_rect);
            } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                data.tray_icon.show_context_menu();
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let cmd_id = (wparam.0 as usize) & 0xFFFF;
            match cmd_id {
                IDM_SHOW_UI => {
                    data.app_state.refresh_devices();
                    let device_items: Vec<DeviceItem> =
                        data.app_state.devices.iter().cloned().map(Into::into).collect();
                    let app_items: Vec<AppAudioItem> =
                        get_active_app_sessions(&data.app_state.devices)
                            .into_iter()
                            .map(Into::into)
                            .collect();
                    let tray_rect = data.tray_icon.get_rect();
                    toggle_flyout(hwnd, device_items, app_items, tray_rect);
                    LRESULT(0)
                }
                IDM_APPLY_RULES => {
                    data.app_state.refresh_devices();
                    data.app_state.reload_rules();
                    let _ = data.app_state.apply_all_rules(true);
                    LRESULT(0)
                }
                IDM_STARTUP => {
                    let current = crate::ui::tray::is_run_at_startup();
                    let _ = crate::ui::tray::set_run_at_startup(!current);
                    LRESULT(0)
                }
                IDM_EXIT => {
                    close_flyout();
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                _ => DefWindowProcW(hwnd, msg, wparam, lparam),
            }
        }

        WM_APP_DEVICE_HOTPLUG | WM_APP_DEFAULT_CHANGED => {
            // Hardware endpoint state changed or default device switched.
            // Reset the 150ms debounce timer to prevent RPC thread congestion.
            let _ = SetTimer(hwnd, TIMER_ID_DEBOUNCE, DEBOUNCE_DELAY_MS, None);
            data.app_state.debounce_timer_active = true;
            LRESULT(0)
        }

        WM_APP_SESSION_CREATED => {
            // New application stream started.
            let pid = wparam.0 as u32;
            if pid > 0 {
                data.app_state.apply_rule_for_pid(pid);
            }
            LRESULT(0)
        }

        WM_APP_APPLY_RULES => {
            let pid = wparam.0 as u32;
            data.app_state.reload_rules();
            if pid > 0 {
                data.app_state.apply_rule_for_pid(pid);
            } else {
                data.app_state.apply_all_rules(true);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            if wparam.0 == TIMER_ID_DEBOUNCE {
                let _ = KillTimer(hwnd, TIMER_ID_DEBOUNCE);
                data.app_state.debounce_timer_active = false;
                data.app_state.refresh_devices();
                data.app_state.apply_all_rules(false);
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            let _ = data.tray_icon.remove();
            let _ = Box::from_raw(data_ptr);
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn main() -> windows::core::Result<()> {
    // 1. Single Instance Guard via Win32 Named Mutex
    let mutex_name = w!("AudioRouterApp_SingleInstance");
    let mutex_handle = unsafe { CreateMutexW(None, true, mutex_name) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return Ok(());
    }

    // 2. Set Per-Monitor DPI Awareness V2 for sharp High-DPI UI rendering
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 3. Initialize COM MTA/STA (APARTMENTTHREADED is ideal for Shell_NotifyIcon and message pump)
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }

    // 4. Register Dispatcher Hidden Window Class
    let instance = unsafe { GetModuleHandleW(None).unwrap_or_default() };
    let class_name = w!("AudioRouterDispatcherClass");

    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(dispatcher_wndproc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassExW(&wc);
    }

    // 5. Create Top-Level Hidden Dispatcher Window (0x0, WS_POPUP, NOT HWND_MESSAGE to capture TaskbarCreated)
    let hwnd = unsafe {
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            class_name,
            w!("AudioRouterDispatcher"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            HWND::default(),
            windows::Win32::UI::WindowsAndMessaging::HMENU::default(),
            instance,
            None,
        )?
    };

    // 6. Initialize AppState, TrayIcon, and Taskbar message subscription
    let mut app_state = AppState::new(hwnd)?;
    let mut tray_icon = TrayIcon::new(hwnd, WM_APP_TRAY, "Audio Router")?;
    let _ = tray_icon.add();
    let taskbar_created_msg = register_taskbar_created_message();

    // Initial pass to enforce active rules on current running audio sessions
    let _ = app_state.apply_all_rules(false);

    // Attach state to window user data
    let dispatcher_data = Box::new(DispatcherData {
        app_state,
        tray_icon,
        taskbar_created_msg,
    });
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(dispatcher_data) as isize);
    }

    // 7. Message Loop
    let mut msg = MSG::default();
    unsafe {
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }

    // 8. Cleanup COM & Mutex
    unsafe {
        CoUninitialize();
        if let Ok(h) = mutex_handle {
            if !h.is_invalid() {
                let _ = CloseHandle(h);
            }
        }
    }

    Ok(())
}
