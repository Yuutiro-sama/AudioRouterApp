//! Win32 High-DPI Ephemeral Dark Flyout Window.
//!
//! Provides simultaneous multi-output selection, independent per-device volume sliders,
//! and per-application routing & volume controls in Windows 11 Fluent Dark Mode style.

use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    CreateFontW, CreateSolidBrush, DeleteObject, GetMonitorInfoW, MonitorFromPoint,
    SetBkColor, SetBkMode, SetTextColor, UpdateWindow, HBRUSH, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, TRANSPARENT,
};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES, INITCOMMONCONTROLSEX,
    TBM_SETPOS, TBM_SETRANGE, TBS_AUTOTICKS, TBS_HORZ,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::Shell::ExtractIconExW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyIcon, DestroyWindow, GetCursorPos,
    GetDlgCtrlID, GetWindowLongPtrW, GetWindowThreadProcessId, KillTimer, PostMessageW,
    RegisterClassExW, SendMessageW, SetForegroundWindow, SetTimer, SetWindowLongPtrW,
    SetWindowPos, SetWindowTextW, ShowWindow,
    BM_GETCHECK, BM_SETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_PUSHBUTTON,
    CBS_DROPDOWNLIST, CBN_CLOSEUP, CBN_DROPDOWN,
    CBN_SELCHANGE, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, HICON, HMENU, STM_SETICON, SW_SHOW, WA_INACTIVE, WINDOW_STYLE,
    WM_ACTIVATE, WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLORSTATIC,
    WM_DESTROY, WM_HSCROLL, WM_KEYDOWN, WM_SETFONT, WM_TIMER, WNDCLASSEXW,
    WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};

const BST_UNCHECKED: usize = 0;
const BST_CHECKED: usize = 1;

use crate::config::rules::{load_config_v3, save_config_v3};
use crate::ipc_messages::WM_APP_APPLY_RULES;

static CURRENT_FLYOUT: AtomicUsize = AtomicUsize::new(0);

const TBM_GETPOS: u32 = 0x0400; // WM_USER
const TIMER_ID_STATS: usize = 1;

const SS_LEFT: WINDOW_STYLE = WINDOW_STYLE(0x00000000);
const SS_CENTER: WINDOW_STYLE = WINDOW_STYLE(0x00000001);
const SS_RIGHT: WINDOW_STYLE = WINDOW_STYLE(0x00000002);
const SS_ICON: WINDOW_STYLE = WINDOW_STYLE(0x00000003);

/// Trims the working set of the current process to aggressively release memory back to the OS (< 2.5 MB)
pub fn trim_background_memory() {
    unsafe {
        let _ = K32EmptyWorkingSet(GetCurrentProcess());
    }
}

/// Item representing an audio endpoint device
#[derive(Clone, Debug, PartialEq)]
pub struct DeviceItem {
    pub id: String,
    pub name: String,
    pub is_default: bool,
    pub is_enabled: bool,
    pub volume: f32, // 0.0 .. 1.0
    pub is_muted: bool,
    pub latency_ms: f32,
    pub drift_ppm: f32,
}

impl DeviceItem {
    pub fn new(id: String, name: String) -> Self {
        Self {
            id,
            name,
            is_default: false,
            is_enabled: false,
            volume: 1.0,
            is_muted: false,
            latency_ms: 0.0,
            drift_ppm: 0.0,
        }
    }
}

/// Item representing an active audio application session
#[derive(Clone, Debug, PartialEq)]
pub struct AppAudioItem {
    pub pid: u32,
    pub name: String,
    pub exe_path: Option<String>,
    pub current_device_id: String,
    pub volume: f32, // 0.0 .. 1.0
    pub is_muted: bool,
}

struct DeviceControlBinding {
    device_index: usize,
    chk_hwnd: HWND,
    badge_hwnd: HWND,
    mute_hwnd: HWND,
    slider_hwnd: HWND,
    percent_hwnd: HWND,
}

struct AppControlBinding {
    app_index: usize,
    combo_hwnd: HWND,
    mute_hwnd: HWND,
    slider_hwnd: HWND,
    percent_hwnd: HWND,
    icon: Option<HICON>,
}

struct FlyoutState {
    owner_hwnd: HWND,
    devices: Vec<DeviceItem>,
    apps: Vec<AppAudioItem>,
    bg_brush: HBRUSH,
    card_brush: HBRUSH,
    title_font: HFONT,
    section_font: HFONT,
    app_font: HFONT,
    small_font: HFONT,
    is_dropdown_open: bool,
    device_bindings: Vec<DeviceControlBinding>,
    app_bindings: Vec<AppControlBinding>,
}

unsafe fn create_ui_font(height: i32, weight: i32) -> HFONT {
    CreateFontW(
        height,
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        0,
        0,
        0,
        5, // CLEARTYPE_QUALITY
        0,
        w!("Segoe UI Variable Text"),
    )
}

fn load_app_icon(exe_path: Option<&str>) -> Option<HICON> {
    if let Some(path) = exe_path {
        let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
        let mut large_icon = HICON::default();
        let mut small_icon = HICON::default();
        unsafe {
            let count = ExtractIconExW(
                PCWSTR(wide.as_ptr()),
                0,
                Some(&mut large_icon),
                Some(&mut small_icon),
                1,
            );
            if count > 0 && !large_icon.is_invalid() {
                if !small_icon.is_invalid() {
                    let _ = DestroyIcon(small_icon);
                }
                return Some(large_icon);
            } else if count > 0 && !small_icon.is_invalid() {
                return Some(small_icon);
            }
        }
    }
    None
}

/// Calculates popup window position right above the system tray or cursor
pub fn calculate_flyout_position(width: i32, height: i32, tray_rect: Option<RECT>) -> (i32, i32) {
    unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);

        let monitor = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(monitor, &mut mi);
        let work = mi.rcWork;

        let margin = 12;
        let (mut x, mut y) = match tray_rect {
            Some(tr) => {
                let center_x = (tr.left + tr.right) / 2;
                let cx = center_x - (width / 2);
                let cy = tr.top - height - 8;
                (cx, cy)
            }
            None => (pt.x - width / 2, pt.y - height - 8),
        };

        if x + width > work.right - margin {
            x = work.right - width - margin;
        }
        if x < work.left + margin {
            x = work.left + margin;
        }
        if y + height > work.bottom - margin {
            y = work.bottom - height - margin;
        }
        if y < work.top + margin {
            y = work.top + margin;
        }

        (x, y)
    }
}

/// Initializes Win32 Common Controls for Trackbar / Slider
pub fn ensure_common_controls() {
    let icce = INITCOMMONCONTROLSEX {
        dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
        dwICC: ICC_BAR_CLASSES | ICC_STANDARD_CLASSES,
    };
    unsafe {
        let _ = InitCommonControlsEx(&icce);
    }
}

/// Closes the current flyout if visible
pub fn close_flyout() {
    let prev = CURRENT_FLYOUT.swap(0, Ordering::SeqCst);
    if prev != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(prev as _));
        }
        trim_background_memory();
    }
}

/// Toggles the flyout window: closes if visible, otherwise creates and displays it
pub fn toggle_flyout(
    owner_hwnd: HWND,
    mut devices: Vec<DeviceItem>,
    mut apps: Vec<AppAudioItem>,
    tray_rect: Option<RECT>,
) {
    let prev = CURRENT_FLYOUT.load(Ordering::SeqCst);
    if prev != 0 {
        close_flyout();
        return;
    }

    ensure_common_controls();

    // 1. Synchronize configuration and device states
    let config = load_config_v3();
    let default_dev_id = crate::audio::get_default_render_device()
        .ok()
        .flatten()
        .map(|d| d.id);

    for dev in &mut devices {
        if let Some(ref def_id) = default_dev_id {
            if &dev.id == def_id {
                dev.is_default = true;
            }
        }
        if config.enabled_devices.is_empty() && dev.is_default {
            dev.is_enabled = true;
        } else {
            dev.is_enabled = config.is_device_enabled(&dev.id);
        }

        if let Some((vol, mute)) = crate::audio::get_device_endpoint_volume(&dev.id) {
            dev.volume = vol;
            dev.is_muted = mute;
        } else {
            dev.volume = config.get_device_volume(&dev.id);
            dev.is_muted = config.is_device_muted(&dev.id);
        }
    }

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

    let class_name = w!("AudioRouterFlyoutClass");
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(flyout_wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hbrBackground: CreateSolidBrush(COLORREF(0x001E1E1E)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        // Preliminary window to determine monitor DPI
        let dummy_width = 420;
        let dummy_height = 300;
        let (pos_x, pos_y) = calculate_flyout_position(dummy_width, dummy_height, tray_rect);

        let hwnd = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
            class_name,
            w!("Audio Router Flyout"),
            WS_POPUP | WS_CLIPCHILDREN | WS_CLIPSIBLINGS,
            pos_x,
            pos_y,
            dummy_width,
            dummy_height,
            HWND::default(),
            HMENU::default(),
            instance,
            None,
        );

        let hwnd = match hwnd {
            Ok(h) => h,
            Err(_) => return,
        };

        CURRENT_FLYOUT.store(hwnd.0 as usize, Ordering::SeqCst);

        // Windows 11 DWM attributes: Rounded corners and Immersive Dark Mode
        let corner_pref = DWMWCP_ROUND.0;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_pref as *const _ as _,
            std::mem::size_of_val(&corner_pref) as u32,
        );

        let dark_mode = windows::Win32::Foundation::BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_mode as *const _ as _,
            std::mem::size_of_val(&dark_mode) as u32,
        );

        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0f32 } else { dpi as f32 / 96.0f32 };

        let width = (420.0 * scale) as i32;
        let pad = (12.0 * scale) as i32;
        let title_h = (28.0 * scale) as i32;
        let sec_h = (22.0 * scale) as i32;
        let dev_row_h = (52.0 * scale) as i32;
        let app_card_h = (82.0 * scale) as i32;

        let dev_section_h = sec_h + (devices.len().max(1) as i32 * dev_row_h) + (8.0 * scale) as i32;
        let app_section_h = if apps.is_empty() {
            sec_h + (52.0 * scale) as i32
        } else {
            sec_h + (apps.len() as i32 * (app_card_h + pad / 3)) + pad
        };

        let calculated_height = title_h + pad * 2 + dev_section_h + app_section_h + (12.0 * scale) as i32;
        let total_height = calculated_height.min((680.0 * scale) as i32);

        let (final_x, final_y) = calculate_flyout_position(width, total_height, tray_rect);
        let _ = SetWindowPos(
            hwnd,
            HWND::default(),
            final_x,
            final_y,
            width,
            total_height,
            windows::Win32::UI::WindowsAndMessaging::SET_WINDOW_POS_FLAGS(0),
        );

        // Fonts
        let title_font = create_ui_font((16.0 * scale) as i32, 700);
        let section_font = create_ui_font((12.0 * scale) as i32, 600);
        let app_font = create_ui_font((13.0 * scale) as i32, 600);
        let small_font = create_ui_font((11.0 * scale) as i32, 400);

        let bg_brush = CreateSolidBrush(COLORREF(0x001E1E1E));
        let card_brush = CreateSolidBrush(COLORREF(0x002B2B2B));

        // 1. Header Title: AUDIO ROUTER v3.0.0
        let title_ctrl = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("AUDIO ROUTER v3.0.0 - MULTI-OUTPUT ENGINE"),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            pad,
            pad,
            width - pad * 2,
            title_h,
            hwnd,
            HMENU::default(),
            instance,
            None,
        );
        if let Ok(th) = title_ctrl {
            let _ = SendMessageW(th, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1));
        }

        let mut cur_y = pad + title_h + (4.0 * scale) as i32;

        // 2. SECTION 1: ĐA THIẾT BỊ ĐẦU RA (MULTI-OUTPUT SELECTION)
        let sec1_ctrl = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("ĐA THIẾT BỊ ĐẦU RA (PHÁT ĐỒNG THỜI)"),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            pad,
            cur_y,
            width - pad * 2,
            sec_h,
            hwnd,
            HMENU(500 as _),
            instance,
            None,
        );
        if let Ok(sh) = sec1_ctrl {
            let _ = SendMessageW(sh, WM_SETFONT, WPARAM(section_font.0 as usize), LPARAM(1));
        }
        cur_y += sec_h + (2.0 * scale) as i32;

        let mut device_bindings = Vec::new();

        if devices.is_empty() {
            let no_dev_ctrl = CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Không tìm thấy thiết bị âm thanh đầu ra khả dụng."),
                WS_CHILD | WS_VISIBLE | SS_LEFT,
                pad + (8.0 * scale) as i32,
                cur_y,
                width - pad * 2,
                (24.0 * scale) as i32,
                hwnd,
                HMENU::default(),
                instance,
                None,
            );
            if let Ok(nh) = no_dev_ctrl {
                let _ = SendMessageW(nh, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));
            }
            cur_y += (28.0 * scale) as i32;
        } else {
            for (i, dev) in devices.iter().enumerate() {
                let row_y = cur_y;
                let base_id = 1000 + i * 10;
                let content_w = width - pad * 2;

                // Line 1: Checkbox + Device Name
                let chk_w = content_w - (140.0 * scale) as i32;
                let dev_name_wide: Vec<u16> = dev.name.encode_utf16().chain(std::iter::once(0)).collect();
                let chk_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    PCWSTR(dev_name_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                    pad + (4.0 * scale) as i32,
                    row_y,
                    chk_w,
                    (22.0 * scale) as i32,
                    hwnd,
                    HMENU(base_id as _),
                    instance,
                    None,
                );
                let chk_hwnd = chk_ctrl.unwrap_or_default();
                let _ = SendMessageW(chk_hwnd, WM_SETFONT, WPARAM(app_font.0 as usize), LPARAM(1));
                let chk_val = if dev.is_enabled { BST_CHECKED as isize } else { BST_UNCHECKED as isize };
                let _ = SendMessageW(chk_hwnd, BM_SETCHECK, WPARAM(chk_val as usize), LPARAM(0));

                // Line 1 Right: Badge [Chính] / [Đang phát] / [Tắt] + Latency/Drift
                let badge_x = pad + (4.0 * scale) as i32 + chk_w;
                let badge_w = (136.0 * scale) as i32;
                let badge_str = if dev.is_enabled {
                    if dev.is_default {
                        "[Chính • 0.0 ms]".to_string()
                    } else if dev.latency_ms > 0.0 {
                        format!("[Đang phát • {:.1}ms | {:+.0}ppm]", dev.latency_ms, dev.drift_ppm)
                    } else {
                        "[Đang phát]".to_string()
                    }
                } else {
                    "[Tắt]".to_string()
                };
                let badge_wide: Vec<u16> = badge_str.encode_utf16().chain(std::iter::once(0)).collect();
                let badge_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(badge_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_RIGHT,
                    badge_x,
                    row_y + 2,
                    badge_w,
                    (20.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 1) as _),
                    instance,
                    None,
                );
                let badge_hwnd = badge_ctrl.unwrap_or_default();
                let _ = SendMessageW(badge_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                // Line 2: Mute Button + Trackbar Slider + Percent Label
                let line2_y = row_y + (24.0 * scale) as i32;
                let btn_w = (48.0 * scale) as i32;
                let pct_w = (38.0 * scale) as i32;
                let slider_w = content_w - btn_w - pct_w - (16.0 * scale) as i32;

                // Mute button
                let mute_text = if dev.is_muted { w!("Bật") } else { w!("Tắt") };
                let mute_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    mute_text,
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    pad + (12.0 * scale) as i32,
                    line2_y,
                    btn_w,
                    (22.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 2) as _),
                    instance,
                    None,
                );
                let mute_hwnd = mute_ctrl.unwrap_or_default();
                let _ = SendMessageW(mute_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                // Trackbar slider
                let slider_x = pad + (12.0 * scale) as i32 + btn_w + (6.0 * scale) as i32;
                let slider_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("msctls_trackbar32"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(TBS_HORZ as u32 | TBS_AUTOTICKS as u32),
                    slider_x,
                    line2_y,
                    slider_w,
                    (22.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 3) as _),
                    instance,
                    None,
                );
                let slider_hwnd = slider_ctrl.unwrap_or_default();
                let _ = SendMessageW(
                    slider_hwnd,
                    TBM_SETRANGE,
                    WPARAM(1),
                    LPARAM(((100u32 << 16) | 0u32) as isize),
                );
                let vol_val = (dev.volume * 100.0).clamp(0.0, 100.0) as u32;
                let _ = SendMessageW(slider_hwnd, TBM_SETPOS, WPARAM(1), LPARAM(vol_val as isize));

                // Percent label
                let pct_x = slider_x + slider_w + (4.0 * scale) as i32;
                let pct_str = format!("{}%", vol_val);
                let pct_wide: Vec<u16> = pct_str.encode_utf16().chain(std::iter::once(0)).collect();
                let pct_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(pct_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_CENTER,
                    pct_x,
                    line2_y + 2,
                    pct_w,
                    (20.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 4) as _),
                    instance,
                    None,
                );
                let percent_hwnd = pct_ctrl.unwrap_or_default();
                let _ = SendMessageW(percent_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                device_bindings.push(DeviceControlBinding {
                    device_index: i,
                    chk_hwnd,
                    badge_hwnd,
                    mute_hwnd,
                    slider_hwnd,
                    percent_hwnd,
                });

                cur_y += dev_row_h;
            }
        }

        cur_y += (6.0 * scale) as i32;

        // 3. SECTION 2: DANH SÁCH ỨNG DỤNG ĐANG PHÁT (ACTIVE SESSIONS)
        let sec2_ctrl = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("ÂM LƯỢNG & ĐỊNH TUYẾN TỪNG ỨNG DỤNG (ACTIVE SESSIONS)"),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            pad,
            cur_y,
            width - pad * 2,
            sec_h,
            hwnd,
            HMENU(501 as _),
            instance,
            None,
        );
        if let Ok(sh) = sec2_ctrl {
            let _ = SendMessageW(sh, WM_SETFONT, WPARAM(section_font.0 as usize), LPARAM(1));
        }
        cur_y += sec_h + (4.0 * scale) as i32;

        let mut app_bindings = Vec::new();

        if apps.is_empty() {
            let empty_ctrl = CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Chưa có ứng dụng nào phát âm thanh.\nHãy mở nhạc, video hoặc game rồi bấm lại icon."),
                WS_CHILD | WS_VISIBLE | SS_CENTER,
                pad,
                cur_y + (8.0 * scale) as i32,
                width - pad * 2,
                (42.0 * scale) as i32,
                hwnd,
                HMENU::default(),
                instance,
                None,
            );
            if let Ok(eh) = empty_ctrl {
                let _ = SendMessageW(eh, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));
            }
        } else {
            for (i, app) in apps.iter().enumerate() {
                let card_y = cur_y;
                let card_w = width - pad * 2;
                let base_id = 2000 + i * 10;

                // App icon
                let icon = load_app_icon(app.exe_path.as_deref());
                let icon_size = (24.0 * scale) as i32;

                if let Some(hicon) = icon {
                    let icon_ctrl = CreateWindowExW(
                        windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                        w!("STATIC"),
                        PCWSTR::null(),
                        WS_CHILD | WS_VISIBLE | SS_ICON,
                        pad + 4,
                        card_y + 4,
                        icon_size,
                        icon_size,
                        hwnd,
                        HMENU(base_id as _),
                        instance,
                        None,
                    );
                    if let Ok(ih) = icon_ctrl {
                        let _ = SendMessageW(ih, STM_SETICON, WPARAM(hicon.0 as usize), LPARAM(0));
                    }
                }

                // App Name Label
                let name_x = pad + icon_size + (8.0 * scale) as i32;
                let name_w = card_w - (icon_size + (8.0 * scale) as i32) - 4;
                let name_display = format!("{} (PID: {})", app.name, app.pid);
                let name_wide: Vec<u16> = name_display.encode_utf16().chain(std::iter::once(0)).collect();
                let name_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(name_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_LEFT,
                    name_x,
                    card_y + 4,
                    name_w,
                    (20.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 1) as _),
                    instance,
                    None,
                );
                if let Ok(nh) = name_ctrl {
                    let _ = SendMessageW(nh, WM_SETFONT, WPARAM(app_font.0 as usize), LPARAM(1));
                }

                // ComboBox: Route selection ("Phát Đa Đầu Ra" or pin to specific device)
                let combo_y = card_y + (26.0 * scale) as i32;
                let combo_h = (26.0 * scale) as i32;
                let combo_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("COMBOBOX"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                    pad + 4,
                    combo_y,
                    card_w - 8,
                    (220.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 2) as _),
                    instance,
                    None,
                );
                let combo_hwnd = combo_ctrl.unwrap_or_default();
                let _ = SendMessageW(combo_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                // Populate ComboBox options
                // Item 0: Multi-Output mode
                let multi_str = w!("🔊 Phát Đa Đầu Ra (Mặc định)");
                let _ = SendMessageW(combo_hwnd, CB_ADDSTRING, WPARAM(0), LPARAM(multi_str.as_ptr() as _));

                let mut selected_idx = 0;
                let is_explicit_multi = app.current_device_id == "multi"
                    || config.get_app_rule(&app.name).map_or(false, |r| r == "multi");

                for (dev_idx, dev) in devices.iter().enumerate() {
                    let opt_name = format!("Ghim: {}", dev.name);
                    let opt_wide: Vec<u16> = opt_name.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SendMessageW(
                        combo_hwnd,
                        CB_ADDSTRING,
                        WPARAM(0),
                        LPARAM(opt_wide.as_ptr() as _),
                    );

                    if !is_explicit_multi && dev.id == app.current_device_id {
                        selected_idx = dev_idx + 1;
                    }
                }
                let _ = SendMessageW(combo_hwnd, CB_SETCURSEL, WPARAM(selected_idx), LPARAM(0));

                // Row 3: Mute button, Slider trackbar, Volume percent
                let row3_y = combo_y + combo_h + (4.0 * scale) as i32;
                let btn_w = (48.0 * scale) as i32;
                let pct_w = (38.0 * scale) as i32;
                let slider_w = card_w - btn_w - pct_w - (16.0 * scale) as i32;

                // Mute button
                let mute_text = if app.is_muted { w!("Bật") } else { w!("Tắt") };
                let mute_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    mute_text,
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_PUSHBUTTON as u32),
                    pad + 4,
                    row3_y,
                    btn_w,
                    (22.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 3) as _),
                    instance,
                    None,
                );
                let mute_hwnd = mute_ctrl.unwrap_or_default();
                let _ = SendMessageW(mute_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                // Slider trackbar
                let slider_x = pad + 4 + btn_w + (4.0 * scale) as i32;
                let slider_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("msctls_trackbar32"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(TBS_HORZ as u32 | TBS_AUTOTICKS as u32),
                    slider_x,
                    row3_y,
                    slider_w,
                    (22.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 4) as _),
                    instance,
                    None,
                );
                let slider_hwnd = slider_ctrl.unwrap_or_default();
                let _ = SendMessageW(
                    slider_hwnd,
                    TBM_SETRANGE,
                    WPARAM(1),
                    LPARAM(((100u32 << 16) | 0u32) as isize),
                );
                let vol_val = (app.volume * 100.0).clamp(0.0, 100.0) as u32;
                let _ = SendMessageW(slider_hwnd, TBM_SETPOS, WPARAM(1), LPARAM(vol_val as isize));

                // Percent label
                let pct_x = slider_x + slider_w + (4.0 * scale) as i32;
                let pct_str = format!("{}%", vol_val);
                let pct_wide: Vec<u16> = pct_str.encode_utf16().chain(std::iter::once(0)).collect();
                let pct_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(pct_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_CENTER,
                    pct_x,
                    row3_y + 2,
                    pct_w,
                    (20.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 5) as _),
                    instance,
                    None,
                );
                let percent_hwnd = pct_ctrl.unwrap_or_default();
                let _ = SendMessageW(percent_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                app_bindings.push(AppControlBinding {
                    app_index: i,
                    combo_hwnd,
                    mute_hwnd,
                    slider_hwnd,
                    percent_hwnd,
                    icon,
                });

                cur_y += app_card_h + (pad / 3);
            }
        }

        // Timer for real-time stats (latency, clock drift)
        let _ = SetTimer(hwnd, TIMER_ID_STATS, 250, None);

        let state = Box::new(FlyoutState {
            owner_hwnd,
            devices,
            apps,
            bg_brush,
            card_brush,
            title_font,
            section_font,
            app_font,
            small_font,
            is_dropdown_open: false,
            device_bindings,
            app_bindings,
        });

        SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }
}

unsafe extern "system" fn flyout_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut FlyoutState;

    match msg {
        WM_ACTIVATE => {
            let active_state = (wparam.0 as u32) & 0xFFFF;
            if active_state == WA_INACTIVE as u32 {
                let other_hwnd = HWND(lparam.0 as _);
                if !other_hwnd.is_invalid() && !other_hwnd.0.is_null() {
                    let mut target_pid = 0u32;
                    unsafe {
                        GetWindowThreadProcessId(other_hwnd, Some(&mut target_pid));
                    }
                    let current_pid = unsafe { GetCurrentProcessId() };
                    if target_pid == current_pid {
                        // Focus is inside our own window or child control (e.g. ComboBox dropdown, slider, button)
                        return LRESULT(0);
                    }
                }

                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    if state.is_dropdown_open {
                        return LRESULT(0);
                    }
                }

                close_flyout();
                return LRESULT(0);
            }
        }

        WM_KEYDOWN => {
            if wparam.0 == VK_ESCAPE.0 as usize {
                close_flyout();
                return LRESULT(0);
            }
        }

        WM_COMMAND => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let notify_code = ((wparam.0 as u32) >> 16) & 0xFFFF;

                if notify_code == CBN_DROPDOWN as u32 {
                    state.is_dropdown_open = true;
                } else if notify_code == CBN_CLOSEUP as u32 {
                    state.is_dropdown_open = false;
                } else if notify_code == CBN_SELCHANGE as u32 {
                    // App routing ComboBox changed
                    for b in &state.app_bindings {
                        if b.combo_hwnd.0 as isize == lparam.0 {
                            let sel = SendMessageW(b.combo_hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                            let app = &mut state.apps[b.app_index];
                            let mut config = load_config_v3();

                            if sel == 0 {
                                // "Phát Đa Đầu Ra" selected
                                app.current_device_id = "multi".to_string();
                                config.set_app_rule(app.name.clone(), "multi".into());
                                let _ = save_config_v3(&config);

                                // Reset app policy route to default so loopback picks it up
                                if let Ok(factory) = crate::audio::policy_config::get_policy_config_factory() {
                                    let _ = crate::audio::policy_config::set_app_output_device(
                                        &factory,
                                        app.pid,
                                        windows::Win32::Media::Audio::eRender,
                                        None,
                                    );
                                    let sibling_pids = crate::audio::enumerator::get_pids_by_process_name(&app.name);
                                    for pid in sibling_pids {
                                        if pid != app.pid {
                                            let _ = crate::audio::policy_config::set_app_output_device(
                                                &factory,
                                                pid,
                                                windows::Win32::Media::Audio::eRender,
                                                None,
                                            );
                                        }
                                    }
                                }
                            } else if sel - 1 < state.devices.len() {
                                // Specific device pinned
                                let dev = &state.devices[sel - 1];
                                app.current_device_id = dev.id.clone();
                                config.set_app_rule(app.name.clone(), dev.id.clone());
                                let _ = save_config_v3(&config);

                                if let Ok(factory) = crate::audio::policy_config::get_policy_config_factory() {
                                    let _ = crate::audio::policy_config::set_app_output_device(
                                        &factory,
                                        app.pid,
                                        windows::Win32::Media::Audio::eRender,
                                        Some(&dev.id),
                                    );
                                    let sibling_pids = crate::audio::enumerator::get_pids_by_process_name(&app.name);
                                    for pid in sibling_pids {
                                        if pid != app.pid {
                                            let _ = crate::audio::policy_config::set_app_output_device(
                                                &factory,
                                                pid,
                                                windows::Win32::Media::Audio::eRender,
                                                Some(&dev.id),
                                            );
                                        }
                                    }
                                }
                            }

                            let _ = PostMessageW(
                                state.owner_hwnd,
                                WM_APP_APPLY_RULES,
                                WPARAM(app.pid as usize),
                                LPARAM(0),
                            );
                            break;
                        }
                    }
                } else if notify_code == BN_CLICKED as u32 {
                    // Check if it is a Device Checkbox
                    for b in &mut state.device_bindings {
                        if b.chk_hwnd.0 as isize == lparam.0 {
                            let is_checked = SendMessageW(b.chk_hwnd, BM_GETCHECK, WPARAM(0), LPARAM(0)).0
                                == BST_CHECKED as isize;
                            let dev = &mut state.devices[b.device_index];
                            dev.is_enabled = is_checked;

                            // Update badge text
                            let badge_str = if dev.is_enabled {
                                if dev.is_default {
                                    "[Chính • 0.0 ms]".to_string()
                                } else if dev.latency_ms > 0.0 {
                                    format!("[Đang phát • {:.1}ms | {:+.0}ppm]", dev.latency_ms, dev.drift_ppm)
                                } else {
                                    "[Đang phát]".to_string()
                                }
                            } else {
                                "[Tắt]".to_string()
                            };
                            let badge_wide: Vec<u16> = badge_str.encode_utf16().chain(std::iter::once(0)).collect();
                            let _ = SetWindowTextW(b.badge_hwnd, PCWSTR(badge_wide.as_ptr()));

                            // Persist to ConfigV3
                            let mut config = load_config_v3();
                            config.set_device_enabled(dev.id.clone(), is_checked);
                            let _ = save_config_v3(&config);

                            // Notify application dispatcher
                            let _ = PostMessageW(state.owner_hwnd, WM_APP_APPLY_RULES, WPARAM(0), LPARAM(0));
                            return LRESULT(0);
                        }
                    }

                    // Check if it is a Device Mute Button
                    for b in &mut state.device_bindings {
                        if b.mute_hwnd.0 as isize == lparam.0 {
                            let dev = &mut state.devices[b.device_index];
                            dev.is_muted = !dev.is_muted;
                            let new_text = if dev.is_muted { w!("Bật") } else { w!("Tắt") };
                            let _ = SetWindowTextW(b.mute_hwnd, new_text);

                            // Apply hardware endpoint mute
                            let _ = crate::audio::set_device_endpoint_mute(&dev.id, dev.is_muted);

                            // Persist to ConfigV3
                            let mut config = load_config_v3();
                            config.set_device_mute(dev.id.clone(), dev.is_muted);
                            let _ = save_config_v3(&config);
                            return LRESULT(0);
                        }
                    }

                    // Check if it is an App Mute Button
                    for b in &mut state.app_bindings {
                        if b.mute_hwnd.0 as isize == lparam.0 {
                            let app = &mut state.apps[b.app_index];
                            app.is_muted = !app.is_muted;
                            let new_text = if app.is_muted { w!("Bật") } else { w!("Tắt") };
                            let _ = SetWindowTextW(b.mute_hwnd, new_text);

                            // Set mute in Windows Core Audio
                            let _ = crate::audio::set_app_mute(app.pid, &app.current_device_id, app.is_muted);

                            // Persist to ConfigV3
                            let mut config = load_config_v3();
                            config.set_app_mute(app.name.clone(), app.is_muted);
                            let _ = save_config_v3(&config);

                            let _ = PostMessageW(
                                state.owner_hwnd,
                                WM_APP_APPLY_RULES,
                                WPARAM(app.pid as usize),
                                LPARAM(if app.is_muted { 1 } else { 0 }),
                            );
                            return LRESULT(0);
                        }
                    }
                }
            }
        }

        WM_HSCROLL => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let slider_hwnd = HWND(lparam.0 as _);

                // 1. Device Volume Slider
                for b in &mut state.device_bindings {
                    if b.slider_hwnd == slider_hwnd {
                        let pos = SendMessageW(slider_hwnd, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as u32;
                        let dev = &mut state.devices[b.device_index];
                        let new_vol = pos as f32 / 100.0;
                        dev.volume = new_vol;

                        let text = format!("{}%", pos);
                        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                        let _ = SetWindowTextW(b.percent_hwnd, PCWSTR(wide.as_ptr()));

                        // Zero-zipper hardware volume adjustment
                        let _ = crate::audio::set_device_endpoint_volume(&dev.id, new_vol);

                        // Persist to ConfigV3
                        let mut config = load_config_v3();
                        config.set_device_volume(dev.id.clone(), new_vol);
                        let _ = save_config_v3(&config);
                        return LRESULT(0);
                    }
                }

                // 2. App Volume Slider
                for b in &mut state.app_bindings {
                    if b.slider_hwnd == slider_hwnd {
                        let pos = SendMessageW(slider_hwnd, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as u32;
                        let app = &mut state.apps[b.app_index];
                        let new_vol = pos as f32 / 100.0;
                        app.volume = new_vol;

                        let text = format!("{}%", pos);
                        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                        let _ = SetWindowTextW(b.percent_hwnd, PCWSTR(wide.as_ptr()));

                        // Set volume in Windows Core Audio
                        let _ = crate::audio::set_app_volume(app.pid, &app.current_device_id, new_vol);

                        // Persist to ConfigV3
                        let mut config = load_config_v3();
                        config.set_app_volume(app.name.clone(), new_vol);
                        let _ = save_config_v3(&config);
                        return LRESULT(0);
                    }
                }
            }
        }

        WM_TIMER => {
            if wparam.0 == TIMER_ID_STATS && !state_ptr.is_null() {
                let state = &mut *state_ptr;
                // Periodic refresh of device badges (latency, drift)
                for b in &state.device_bindings {
                    let dev = &state.devices[b.device_index];
                    let badge_str = if dev.is_enabled {
                        if dev.is_default {
                            "[Chính • 0.0 ms]".to_string()
                        } else if dev.latency_ms > 0.0 {
                            format!("[Đang phát • {:.1}ms | {:+.0}ppm]", dev.latency_ms, dev.drift_ppm)
                        } else {
                            "[Đang phát]".to_string()
                        }
                    } else {
                        "[Tắt]".to_string()
                    };
                    let badge_wide: Vec<u16> = badge_str.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SetWindowTextW(b.badge_hwnd, PCWSTR(badge_wide.as_ptr()));
                }
                return LRESULT(0);
            }
        }

        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let hdc = HDC(wparam.0 as _);
                let ctrl_id = GetDlgCtrlID(HWND(lparam.0 as _));

                if ctrl_id == 500 || ctrl_id == 501 {
                    // Section Headers: Cyan Accent (#60CDFF -> BBGGRR: 0x00FFCD60)
                    let _ = SetTextColor(hdc, COLORREF(0x00FFCD60));
                } else if ctrl_id >= 1000 && ctrl_id < 2000 && (ctrl_id % 10 == 1) {
                    // Device status badges: Mint Green (#00E676 -> BBGGRR: 0x0076E600)
                    let _ = SetTextColor(hdc, COLORREF(0x0076E600));
                } else {
                    let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF)); // White text
                }

                let _ = SetBkColor(hdc, COLORREF(0x001E1E1E));  // Dark background
                let _ = SetBkMode(hdc, TRANSPARENT);
                return LRESULT(state.bg_brush.0 as isize);
            }
        }

        WM_DESTROY => {
            let _ = KillTimer(hwnd, TIMER_ID_STATS);
            if !state_ptr.is_null() {
                let state = Box::from_raw(state_ptr);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);

                let _ = DeleteObject(state.bg_brush);
                let _ = DeleteObject(state.card_brush);
                let _ = DeleteObject(state.title_font);
                let _ = DeleteObject(state.section_font);
                let _ = DeleteObject(state.app_font);
                let _ = DeleteObject(state.small_font);

                for b in &state.app_bindings {
                    if let Some(icon) = b.icon {
                        let _ = DestroyIcon(icon);
                    }
                }
            }
            CURRENT_FLYOUT.store(0, Ordering::SeqCst);
            trim_background_memory();
            return LRESULT(0);
        }

        _ => {}
    }

    DefWindowProcW(hwnd, msg, wparam, lparam)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_trim_background_memory() {
        trim_background_memory();
    }

    #[test]
    fn test_device_item_creation() {
        let dev = DeviceItem::new("dev_id".into(), "Headphones".into());
        assert_eq!(dev.id, "dev_id");
        assert_eq!(dev.name, "Headphones");
        assert!(!dev.is_enabled);
        assert_eq!(dev.volume, 1.0);
    }

    #[test]
    fn test_calculate_flyout_pos() {
        let (x, y) = calculate_flyout_position(420, 300, None);
        assert!(x >= 0);
        assert!(y >= 0);

        let dummy_rect = RECT {
            left: 1700,
            top: 1040,
            right: 1740,
            bottom: 1080,
        };
        let (x2, y2) = calculate_flyout_position(420, 300, Some(dummy_rect));
        assert!(x2 > 0);
        assert!(y2 > 0);
    }
}
