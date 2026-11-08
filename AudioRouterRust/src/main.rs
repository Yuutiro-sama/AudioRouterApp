#![windows_subsystem = "windows"]

//! AudioRouterApp - High-Performance Native Rust Audio Router.
//!
//! Sub-3MB binary footprint and ultra-low RAM usage (< 2.5MB idle).
//! Multi-Output DSP Engine, per-application audio routing, independent device and app volume controls.

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
use crate::ipc_messages::{
    DEBOUNCE_DELAY_MS, IDM_APPLY_RULES, IDM_EXIT, IDM_SHOW_UI, IDM_STARTUP,
    TIMER_ID_DEBOUNCE, WM_APP_APPLY_RULES, WM_APP_DEFAULT_CHANGED,
    WM_APP_DEVICE_HOTPLUG, WM_APP_SESSION_CREATED, WM_APP_TRAY,
};
use crate::ui::popup::{close_flyout, toggle_flyout};
use crate::ui::tray::{register_taskbar_created_message, TrayIcon};

/// Context structure stored in the hidden dispatcher window's `GWLP_USERDATA`.
struct DispatcherData {
    app_state: AppState,
    tray_icon: TrayIcon,
    taskbar_created_msg: u32,
    show_ui_msg: u32,
}

static LAST_TRAY_CLICK: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const TIMER_ID_TRAY_RETRY: usize = 1002;

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

    // 2. Check if another instance requested to show the UI
    if msg == data.show_ui_msg && data.show_ui_msg != 0 {
        debug_log("dispatcher_wndproc: show_ui_msg broadcast received");
        data.app_state.refresh_devices();
        let device_items = data.app_state.get_device_items();
        let app_items = data.app_state.get_app_items();
        let tray_rect = data.tray_icon.get_rect();
        toggle_flyout(hwnd, device_items, app_items, tray_rect);
        return LRESULT(0);
    }

    match msg {
        WM_APP_TRAY => {
            let event = (lparam.0 as u32) & 0xFFFF;
            // Left click, button down, or notification select with 250ms debounce
            if event == WM_LBUTTONUP || event == 0x0400 /* NIN_SELECT */ || event == windows::Win32::UI::WindowsAndMessaging::WM_LBUTTONDOWN {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_millis() as u64;
                let last = LAST_TRAY_CLICK.swap(now, std::sync::atomic::Ordering::SeqCst);
                if now.saturating_sub(last) < 250 {
                    return LRESULT(0); // Ignore duplicate shell events within 250ms
                }

                data.app_state.refresh_devices();
                let device_items = data.app_state.get_device_items();
                let app_items = data.app_state.get_app_items();
                let tray_rect = data.tray_icon.get_rect();
                toggle_flyout(hwnd, device_items, app_items, tray_rect);
            } else if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                data.tray_icon.show_context_menu();
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let cmd_id = (wparam.0 as usize) & 0xFFFF;
            debug_log(&format!("dispatcher_wndproc: WM_COMMAND received, cmd_id={}", cmd_id));
            match cmd_id {
                IDM_SHOW_UI => {
                    data.app_state.refresh_devices();
                    let device_items = data.app_state.get_device_items();
                    let app_items = data.app_state.get_app_items();
                    let tray_rect = data.tray_icon.get_rect();
                    debug_log(&format!("Calling toggle_flyout with {} apps, {} devices", app_items.len(), device_items.len()));
                    toggle_flyout(hwnd, device_items, app_items, tray_rect);
                    LRESULT(0)
                }
                IDM_APPLY_RULES => {
                    data.app_state.refresh_devices();
                    data.app_state.reload_rules();
                    data.app_state.sync_multi_engine();
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
            data.app_state.sync_multi_engine();
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
                data.app_state.sync_multi_engine();
                data.app_state.apply_all_rules(false);
            } else if wparam.0 == TIMER_ID_TRAY_RETRY {
                if !data.tray_icon.is_added {
                    if data.tray_icon.add().is_ok() {
                        let _ = KillTimer(hwnd, TIMER_ID_TRAY_RETRY);
                    }
                } else {
                    let _ = KillTimer(hwnd, TIMER_ID_TRAY_RETRY);
                }
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

fn debug_log(msg: &str) {
    if let Ok(dir) = std::env::var("APPDATA") {
        let p = format!("{}\\AudioRouter\\debug.log", dir);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "[{}] {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(), msg)
        });
    }
}

fn main() -> windows::core::Result<()> {
    std::panic::set_hook(Box::new(|info| {
        debug_log(&format!("FATAL PANIC: {:?}", info));
    }));
    debug_log("AudioRouter main() started");
    // 1. Single Instance Guard via Win32 Named Mutex
    let mutex_name = w!("AudioRouterApp_SingleInstance");
    let mutex_handle = unsafe { CreateMutexW(None, true, mutex_name) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        debug_log("Instance already exists, sending IDM_SHOW_UI and exiting");
        unsafe {
            let broadcast_msg = windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(w!("AudioRouter_ShowUI"));
            if broadcast_msg != 0 {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    windows::Win32::UI::WindowsAndMessaging::HWND_BROADCAST,
                    broadcast_msg,
                    WPARAM(0),
                    LPARAM(0),
                );
            }

            let _ = windows::Win32::Foundation::SetLastError(windows::Win32::Foundation::WIN32_ERROR(0));
            let existing_hwnd = windows::Win32::UI::WindowsAndMessaging::FindWindowW(
                w!("AudioRouterDispatcherClass"),
                None,
            );
            if let Ok(h) = existing_hwnd {
                if !h.is_invalid() && !h.0.is_null() {
                    let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                        h,
                        windows::Win32::UI::WindowsAndMessaging::WM_COMMAND,
                        WPARAM(crate::ipc_messages::IDM_SHOW_UI as usize),
                        LPARAM(0),
                    );
                }
            }
        }
        return Ok(());
    }
    debug_log("Mutex acquired");

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

    debug_log("RegisterClassExW done, creating hidden dispatcher window");
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
        )
    };

    let hwnd = match hwnd {
        Ok(h) => {
            debug_log(&format!("Dispatcher window created, hwnd={:?}", h.0));
            h
        }
        Err(e) => {
            debug_log(&format!("CreateWindowExW failed: {:?}", e));
            return Err(e);
        }
    };

    // 6. Initialize AppState, TrayIcon, and Taskbar message subscription
    debug_log("Initializing AppState");
    let mut app_state = match AppState::new(hwnd) {
        Ok(s) => {
            debug_log("AppState initialized successfully");
            s
        }
        Err(e) => {
            debug_log(&format!("AppState::new failed: {:?}", e));
            return Err(e);
        }
    };

    debug_log("Initializing TrayIcon");
    let mut tray_icon = match TrayIcon::new(hwnd, WM_APP_TRAY, "Audio Router") {
        Ok(t) => t,
        Err(e) => {
            debug_log(&format!("TrayIcon::new failed: {:?}", e));
            return Err(e);
        }
    };

    if let Err(e) = tray_icon.add() {
        debug_log(&format!("tray_icon.add() failed: {:?}, will retry via timer", e));
        unsafe {
            let _ = SetTimer(hwnd, TIMER_ID_TRAY_RETRY, 1000, None);
        }
    } else {
        debug_log("tray_icon.add() succeeded!");
    }

    let taskbar_created_msg = register_taskbar_created_message();

    // Initial pass to enforce active rules on current running audio sessions
    let _ = app_state.apply_all_rules(false);

    // Attach state to window user data
    let show_ui_msg = unsafe { windows::Win32::UI::WindowsAndMessaging::RegisterWindowMessageW(w!("AudioRouter_ShowUI")) };
    let dispatcher_data = Box::new(DispatcherData {
        app_state,
        tray_icon,
        taskbar_created_msg,
        show_ui_msg,
    });
    unsafe {
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(dispatcher_data) as isize);
    }

    // If not started with --tray or -m, show the Flyout UI immediately so user sees the interface
    let args: Vec<String> = std::env::args().collect();
    let start_in_tray = args.iter().any(|a| a == "--tray" || a == "--minimized" || a == "-m");
    if !start_in_tray {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WM_COMMAND,
                WPARAM(crate::ipc_messages::IDM_SHOW_UI as usize),
                LPARAM(0),
            );
        }
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
