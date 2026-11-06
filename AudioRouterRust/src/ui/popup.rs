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
    GetWindowLongPtrW, GetWindowThreadProcessId, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    BN_CLICKED, BS_PUSHBUTTON, CBS_DROPDOWNLIST, CBN_CLOSEUP, CBN_DROPDOWN, CBN_SELCHANGE,
    CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HICON,
    HMENU, STM_SETICON, SW_SHOW, WA_INACTIVE, WINDOW_STYLE, WM_ACTIVATE, WM_COMMAND,
    WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLORSTATIC, WM_DESTROY, WM_HSCROLL, WM_KEYDOWN,
    WM_SETFONT, WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};

use crate::config::rules::{load_rules, save_rules};
use crate::ipc_messages::WM_APP_APPLY_RULES;

static CURRENT_FLYOUT: AtomicUsize = AtomicUsize::new(0);

const TBM_GETPOS: u32 = 0x0400; // WM_USER
const SS_LEFT: WINDOW_STYLE = WINDOW_STYLE(0x00000000);
const SS_CENTER: WINDOW_STYLE = WINDOW_STYLE(0x00000001);
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
    app_font: HFONT,
    small_font: HFONT,
    is_dropdown_open: bool,
    bindings: Vec<AppControlBinding>,
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
    devices: Vec<DeviceItem>,
    apps: Vec<AppAudioItem>,
    tray_rect: Option<RECT>,
) {
    let prev = CURRENT_FLYOUT.load(Ordering::SeqCst);
    if prev != 0 {
        close_flyout();
        return;
    }

    ensure_common_controls();

    let class_name = w!("AudioRouterFlyoutClass");
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(flyout_wndproc),
            hInstance: instance.into(),
            lpszClassName: class_name,
            hbrBackground: CreateSolidBrush(COLORREF(0x001F1F1F)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        // Preliminary window to determine DPI
        let dummy_width = 380;
        let dummy_height = 200;
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
            HWND::default(), // Independent top-level window so owner doesn't interfere with focus
            HMENU::default(),
            instance,
            None,
        );

        let hwnd = match hwnd {
            Ok(h) => h,
            Err(_) => return,
        };

        CURRENT_FLYOUT.store(hwnd.0 as usize, Ordering::SeqCst);

        // Windows 11 DWM attributes: Rounded corners and Dark Mode
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

        let width = (380.0 * scale) as i32;
        let pad = (12.0 * scale) as i32;
        let title_h = (28.0 * scale) as i32;
        let card_h = (88.0 * scale) as i32;

        let total_height = if apps.is_empty() {
            (140.0 * scale) as i32
        } else {
            let content_h = title_h + pad + (apps.len() as i32 * (card_h + pad / 2)) + pad;
            content_h.min((560.0 * scale) as i32)
        };

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
        let title_font = create_ui_font((18.0 * scale) as i32, 600);
        let app_font = create_ui_font((14.0 * scale) as i32, 600);
        let small_font = create_ui_font((12.0 * scale) as i32, 400);

        let bg_brush = CreateSolidBrush(COLORREF(0x001F1F1F));
        let card_brush = CreateSolidBrush(COLORREF(0x002B2B2B));

        // Create Header Title Label
        let title_hwnd = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("Audio Router"),
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
        if let Ok(th) = title_hwnd {
            let _ = SendMessageW(th, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1));
        }

        let mut bindings = Vec::new();
        let mut cur_y = pad + title_h + (6.0 * scale) as i32;

        if apps.is_empty() {
            let empty_hwnd = CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Chưa có app nào đang có phiên âm thanh.\nHãy mở nhạc hoặc game rồi bấm lại icon."),
                WS_CHILD | WS_VISIBLE | SS_CENTER,
                pad,
                cur_y + (16.0 * scale) as i32,
                width - pad * 2,
                (48.0 * scale) as i32,
                hwnd,
                HMENU::default(),
                instance,
                None,
            );
            if let Ok(eh) = empty_hwnd {
                let _ = SendMessageW(eh, WM_SETFONT, WPARAM(app_font.0 as usize), LPARAM(1));
            }
        } else {
            for (i, app) in apps.iter().enumerate() {
                let card_y = cur_y;
                let card_w = width - pad * 2;
                let base_id = 1000 + i * 10;

                // App icon
                let icon = load_app_icon(app.exe_path.as_deref());
                let icon_size = (28.0 * scale) as i32;

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

                // App Name
                let name_x = pad + icon_size + (8.0 * scale) as i32;
                let name_w = card_w - (icon_size + (8.0 * scale) as i32) - 4;
                let name_wide: Vec<u16> = app.name.encode_utf16().chain(std::iter::once(0)).collect();
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

                // ComboBox: Device selection
                let combo_y = card_y + (28.0 * scale) as i32;
                let combo_h = (26.0 * scale) as i32;
                let combo_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("COMBOBOX"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                    pad + 4,
                    combo_y,
                    card_w - 8,
                    (200.0 * scale) as i32,
                    hwnd,
                    HMENU((base_id + 2) as _),
                    instance,
                    None,
                );

                let combo_hwnd = combo_ctrl.unwrap_or_default();
                let _ = SendMessageW(combo_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));

                // Populate devices in ComboBox
                let mut selected_idx = 0;
                for (dev_idx, dev) in devices.iter().enumerate() {
                    let dev_wide: Vec<u16> = dev.name.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SendMessageW(
                        combo_hwnd,
                        CB_ADDSTRING,
                        WPARAM(0),
                        LPARAM(dev_wide.as_ptr() as _),
                    );
                    if dev.id == app.current_device_id {
                        selected_idx = dev_idx;
                    }
                }
                let _ = SendMessageW(combo_hwnd, CB_SETCURSEL, WPARAM(selected_idx), LPARAM(0));

                // Row 3: Mute button, Slider trackbar, Volume percent
                let row3_y = combo_y + combo_h + (4.0 * scale) as i32;
                let btn_w = (54.0 * scale) as i32;
                let pct_w = (38.0 * scale) as i32;
                let slider_w = card_w - btn_w - pct_w - (16.0 * scale) as i32;

                // Mute Button
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

                // Slider Trackbar
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

                // Percent Label
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

                bindings.push(AppControlBinding {
                    app_index: i,
                    combo_hwnd,
                    mute_hwnd,
                    slider_hwnd,
                    percent_hwnd,
                    icon,
                });

                cur_y += card_h + (pad / 2);
            }
        }

        let state = Box::new(FlyoutState {
            owner_hwnd,
            devices,
            apps,
            bg_brush,
            card_brush,
            title_font,
            app_font,
            small_font,
            is_dropdown_open: false,
            bindings,
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
                        // Focus is inside our own window or child control (e.g. ComboBox dropdown, slider, button)!
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
                    for b in &state.bindings {
                        if b.combo_hwnd.0 as isize == lparam.0 {
                            let sel = SendMessageW(b.combo_hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                            if sel < state.devices.len() {
                                let dev = &state.devices[sel];
                                let app = &mut state.apps[b.app_index];
                                app.current_device_id = dev.id.clone();

                                // Save rule to %AppData%\AudioRouter\rules.json
                                let mut rules = load_rules();
                                rules.insert(app.name.clone(), dev.id.clone());
                                let _ = save_rules(&rules);

                                // Notify owner window to trigger routing update
                                let _ = PostMessageW(
                                    state.owner_hwnd,
                                    WM_APP_APPLY_RULES,
                                    WPARAM(app.pid as usize),
                                    LPARAM(0),
                                );
                            }
                            break;
                        }
                    }
                } else if notify_code == BN_CLICKED as u32 {
                    for b in &mut state.bindings {
                        if b.mute_hwnd.0 as isize == lparam.0 {
                            let app = &mut state.apps[b.app_index];
                            app.is_muted = !app.is_muted;
                            let new_text = if app.is_muted { w!("Bật") } else { w!("Tắt") };
                            let _ = SetWindowTextW(b.mute_hwnd, new_text);

                            // Set mute in Windows Core Audio
                            let _ = crate::audio::set_app_mute(app.pid, &app.current_device_id, app.is_muted);

                            let _ = PostMessageW(
                                state.owner_hwnd,
                                WM_APP_APPLY_RULES,
                                WPARAM(app.pid as usize),
                                LPARAM(if app.is_muted { 1 } else { 0 }),
                            );
                            break;
                        }
                    }
                }
            }
        }

        WM_HSCROLL => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                let slider_hwnd = HWND(lparam.0 as _);
                for b in &mut state.bindings {
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
                        break;
                    }
                }
            }
        }

        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let hdc = HDC(wparam.0 as _);
                let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF)); // White text
                let _ = SetBkColor(hdc, COLORREF(0x001F1F1F));  // Dark background
                let _ = SetBkMode(hdc, TRANSPARENT);
                return LRESULT(state.bg_brush.0 as isize);
            }
        }

        WM_DESTROY => {
            if !state_ptr.is_null() {
                let state = Box::from_raw(state_ptr);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);

                let _ = DeleteObject(state.bg_brush);
                let _ = DeleteObject(state.card_brush);
                let _ = DeleteObject(state.title_font);
                let _ = DeleteObject(state.app_font);
                let _ = DeleteObject(state.small_font);

                for b in &state.bindings {
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
    fn test_calculate_flyout_pos() {
        let (x, y) = calculate_flyout_position(380, 200, None);
        assert!(x >= 0);
        assert!(y >= 0);

        let dummy_rect = RECT {
            left: 1700,
            top: 1040,
            right: 1740,
            bottom: 1080,
        };
        let (x2, y2) = calculate_flyout_position(380, 200, Some(dummy_rect));
        assert!(x2 > 0);
        assert!(y2 > 0);
    }
}
