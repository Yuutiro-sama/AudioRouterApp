//! Win32 High-DPI Ephemeral Dark Flyout Window.
//!
//! Provides per-application dual output routing, independent secondary toggle,
//! and volume & mute controls in Windows 11 Fluent Dark Mode style.

use std::sync::atomic::{AtomicUsize, Ordering};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWMWCP_ROUND,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, GetMonitorInfoW, InvalidateRect, MonitorFromPoint, RoundRect, SelectObject,
    SetBkColor, SetBkMode, SetTextColor, UpdateWindow, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    HBRUSH, HDC, HFONT, MONITORINFO, MONITOR_DEFAULTTONEAREST, PAINTSTRUCT, PS_SOLID, TRANSPARENT,
};
use windows::Win32::System::ProcessStatus::K32EmptyWorkingSet;
use windows::Win32::System::Threading::{GetCurrentProcess, GetCurrentProcessId};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, SetScrollInfo, DRAWITEMSTRUCT, ICC_BAR_CLASSES, ICC_STANDARD_CLASSES,
    INITCOMMONCONTROLSEX, ODS_SELECTED, TBM_SETPOS, TBM_SETRANGE, TBS_AUTOTICKS, TBS_HORZ,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE};
use windows::Win32::UI::Shell::{DefSubclassProc, ExtractIconExW, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, CreateWindowExW, DefWindowProcW, DeferWindowPos, DestroyIcon,
    DestroyWindow, EndDeferWindowPos, GetClientRect, GetCursorPos, GetDlgCtrlID, GetParent,
    GetScrollInfo, GetWindowLongPtrW, GetWindowThreadProcessId, PostMessageW, RegisterClassExW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW,
    ShowWindow, BN_CLICKED, BS_OWNERDRAW, CBS_DROPDOWNLIST, CBN_CLOSEUP, CBN_DROPDOWN,
    CBN_SELCHANGE, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CS_HREDRAW, CS_VREDRAW,
    GWLP_USERDATA, HICON, HMENU, SB_LINEDOWN, SB_LINEUP, SB_PAGEDOWN, SB_PAGEUP,
    SB_THUMBPOSITION, SB_THUMBTRACK, SB_VERT, SCROLLINFO, SIF_ALL, SIF_PAGE, SIF_POS, SIF_RANGE,
    STM_SETICON, SWP_NOACTIVATE, SWP_NOZORDER, SW_SHOW, WA_INACTIVE, WINDOW_STYLE, WM_ACTIVATE,
    WM_COMMAND, WM_CTLCOLORBTN, WM_CTLCOLORDLG, WM_CTLCOLORSTATIC, WM_DESTROY, WM_DRAWITEM,
    WM_ERASEBKGND, WM_HSCROLL, WM_KEYDOWN, WM_MOUSEWHEEL, WM_PAINT, WM_SETFONT, WM_VSCROLL,
    WNDCLASSEXW, WS_CHILD, WS_CLIPCHILDREN, WS_CLIPSIBLINGS, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_POPUP, WS_VISIBLE, WS_VSCROLL,
};

use crate::config::rules::{load_config_v3, save_config_v3};
use crate::ipc_messages::WM_APP_APPLY_RULES;

static CURRENT_FLYOUT: AtomicUsize = AtomicUsize::new(0);

const TBM_GETPOS: u32 = 0x0400; // WM_USER
const TBM_SETPAGESIZE: u32 = 0x0415; // WM_USER + 21 (step per click on channel: 5%)
const TBM_SETLINESIZE: u32 = 0x0417; // WM_USER + 23 (step per keyboard arrow key: 1%)

const SS_LEFT: WINDOW_STYLE = WINDOW_STYLE(0x00000000);
const SS_CENTER: WINDOW_STYLE = WINDOW_STYLE(0x00000001);
const SS_RIGHT: WINDOW_STYLE = WINDOW_STYLE(0x00000002);
const SS_ICON: WINDOW_STYLE = WINDOW_STYLE(0x00000003);

/// Subclass function to suppress wheel changes on Sliders and ComboBoxes,
/// and forward the wheel event to the parent flyout window for smooth scrolling.
unsafe extern "system" fn no_wheel_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    if msg == WM_MOUSEWHEEL {
        let flyout_raw = CURRENT_FLYOUT.load(Ordering::SeqCst);
        if flyout_raw != 0 {
            return SendMessageW(HWND(flyout_raw as _), msg, wparam, lparam);
        }
        if let Ok(parent) = GetParent(hwnd) {
            if !parent.is_invalid() && !parent.0.is_null() {
                return SendMessageW(parent, msg, wparam, lparam);
            }
        }
        return LRESULT(0);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

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

/// Item representing an active audio application session with dual output support
#[derive(Clone, Debug, PartialEq)]
pub struct AppAudioItem {
    pub pid: u32,
    pub name: String,
    pub exe_path: Option<String>,
    pub primary_device_id: String,
    pub secondary_device_id: String,
    pub secondary_enabled: bool,
    pub volume: f32, // 0.0 .. 1.0
    pub is_muted: bool,
    pub current_device_id: String,
}

impl AppAudioItem {
    pub fn new(pid: u32, name: String) -> Self {
        Self {
            pid,
            name,
            exe_path: None,
            primary_device_id: String::new(),
            secondary_device_id: String::new(),
            secondary_enabled: false,
            volume: 1.0,
            is_muted: false,
            current_device_id: String::new(),
        }
    }
}

struct AppControlBinding {
    app_index: usize,
    combo1_hwnd: HWND,
    combo2_hwnd: HWND,
    toggle2_hwnd: HWND,
    mute_hwnd: HWND,
    slider_hwnd: HWND,
    percent_hwnd: HWND,
    icon: Option<HICON>,
}

#[derive(Clone, Copy)]
struct ChildLayout {
    hwnd: HWND,
    x: i32,
    base_y: i32,
    w: i32,
    h: i32,
}

struct FlyoutState {
    owner_hwnd: HWND,
    container_hwnd: HWND,
    scale: f32,
    scroll_y: i32,
    max_scroll_y: i32,
    container_h: i32,
    card_h: i32,
    card_gap: i32,
    scroll_children: Vec<ChildLayout>,
    devices: Vec<DeviceItem>,
    apps: Vec<AppAudioItem>,
    bg_brush: HBRUSH,
    card_brush: HBRUSH,
    title_font: HFONT,
    app_font: HFONT,
    label_font: HFONT,
    small_font: HFONT,
    pid_font: HFONT,
    is_dropdown_open: bool,
    app_bindings: Vec<AppControlBinding>,
}

impl FlyoutState {
    unsafe fn apply_scroll(&self) {
        if !self.scroll_children.is_empty() {
            if let Ok(mut hdwp) = BeginDeferWindowPos(self.scroll_children.len() as i32) {
                for c in &self.scroll_children {
                    let cur_y = c.base_y - self.scroll_y;
                    if let Ok(next) = DeferWindowPos(
                        hdwp,
                        c.hwnd,
                        HWND::default(),
                        c.x,
                        cur_y,
                        c.w,
                        c.h,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    ) {
                        hdwp = next;
                    }
                }
                let _ = EndDeferWindowPos(hdwp);
            }

            let si = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_POS,
                nPos: self.scroll_y,
                ..Default::default()
            };
            let _ = SetScrollInfo(self.container_hwnd, SB_VERT, &si, BOOL(1));
            let _ = InvalidateRect(self.container_hwnd, None, BOOL(1));
        }
    }
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

/// Container window procedure: delegates events and paints card backgrounds
unsafe extern "system" fn container_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND | WM_HSCROLL | WM_DRAWITEM | WM_MOUSEWHEEL | WM_VSCROLL => {
            if let Ok(parent) = GetParent(hwnd) {
                if !parent.is_invalid() && !parent.0.is_null() {
                    return SendMessageW(parent, msg, wparam, lparam);
                }
            }
        }
        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => {
            if let Ok(parent) = GetParent(hwnd) {
                if !parent.is_invalid() && !parent.0.is_null() {
                    return SendMessageW(parent, msg, wparam, lparam);
                }
            }
        }
        WM_ERASEBKGND => {
            return LRESULT(1);
        }
        WM_PAINT => {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(hwnd, &mut rc);

            let bg_brush = CreateSolidBrush(COLORREF(0x001E1E1E));
            let _ = FillRect(hdc, &rc, bg_brush);
            let _ = DeleteObject(bg_brush);

            if let Ok(parent) = GetParent(hwnd) {
                let state_ptr = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut FlyoutState;
                if !state_ptr.is_null() {
                    let state = &*state_ptr;
                    let pad = (14.0 * state.scale) as i32;
                    let card_w = rc.right - rc.left - pad * 2;
                    let corner = (8.0 * state.scale) as i32;

                    let border_pen = CreatePen(PS_SOLID, 1, COLORREF(0x003A3A3A));
                    let old_pen = SelectObject(hdc, border_pen);
                    let old_brush = SelectObject(hdc, state.card_brush);

                    for i in 0..state.apps.len() {
                        let card_y = i as i32 * (state.card_h + state.card_gap) - state.scroll_y;
                        if card_y + state.card_h >= 0 && card_y <= rc.bottom {
                            let _ = RoundRect(
                                hdc,
                                pad,
                                card_y,
                                pad + card_w,
                                card_y + state.card_h,
                                corner,
                                corner,
                            );
                        }
                    }

                    let _ = SelectObject(hdc, old_pen);
                    let _ = SelectObject(hdc, old_brush);
                    let _ = DeleteObject(border_pen);
                }
            }

            let _ = EndPaint(hwnd, &ps);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// Toggles the flyout window: closes if visible, otherwise creates and displays it
pub fn toggle_flyout(
    owner_hwnd: HWND,
    devices: Vec<DeviceItem>,
    mut apps: Vec<AppAudioItem>,
    tray_rect: Option<RECT>,
) {
    let prev = CURRENT_FLYOUT.load(Ordering::SeqCst);
    if prev != 0 {
        close_flyout();
        return;
    }

    ensure_common_controls();

    // 1. Synchronize configuration and application routing states
    let config = load_config_v3();

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
        if rule.muted || config.is_app_muted(&app.name) {
            app.is_muted = true;
        }
        let saved_vol = config.get_app_volume(&app.name);
        if saved_vol < 1.0 || config.app_volumes.contains_key(&crate::config::rules::normalize_app_key(&app.name)) {
            app.volume = saved_vol;
        }
    }

    let class_name = w!("AudioRouterFlyoutClass");
    let container_class = w!("AudioRouterContainerClass");
    unsafe {
        let instance = windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap_or_default();

        // Register Flyout window class
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

        // Register Container window class
        let cc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(container_wndproc),
            hInstance: instance.into(),
            lpszClassName: container_class,
            hbrBackground: CreateSolidBrush(COLORREF(0x001E1E1E)),
            ..Default::default()
        };
        let _ = RegisterClassExW(&cc);

        // Preliminary window to determine monitor DPI
        let dummy_width = 520;
        let dummy_height = 360;
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

        let dark_mode = BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark_mode as *const _ as _,
            std::mem::size_of_val(&dark_mode) as u32,
        );

        let dpi = GetDpiForWindow(hwnd);
        let scale = if dpi == 0 { 1.0f32 } else { dpi as f32 / 96.0f32 };

        // Width 520px * scale: generous room for full device names and clean controls
        let width = (520.0 * scale) as i32;
        let pad = (14.0 * scale) as i32;
        let title_h = (28.0 * scale) as i32;
        let header_area_h = pad + title_h + (8.0 * scale) as i32;

        let card_h = (166.0 * scale) as i32;
        let card_gap = (12.0 * scale) as i32;
        let card_w = width - pad * 2;
        let inner_pad = (12.0 * scale) as i32;

        let content_height = if apps.is_empty() {
            (90.0 * scale) as i32
        } else {
            apps.len() as i32 * (card_h + card_gap) + pad
        };

        let calculated_height = header_area_h + content_height + (8.0 * scale) as i32;
        let max_height = (650.0 * scale) as i32;
        let total_height = calculated_height.min(max_height);

        let container_y = header_area_h;
        let container_h = total_height - container_y - (6.0 * scale) as i32;
        let max_scroll_y = (content_height - container_h).max(0);

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

        // Typography: Title 16pt Bold, App Name 14pt Bold, Labels 12pt Bold, Text 12pt, PID 11pt
        let title_font = create_ui_font((16.0 * scale) as i32, 700);
        let app_font = create_ui_font((14.0 * scale) as i32, 700);
        let label_font = create_ui_font((12.0 * scale) as i32, 700);
        let small_font = create_ui_font((12.0 * scale) as i32, 500);
        let pid_font = create_ui_font((11.0 * scale) as i32, 400);

        let bg_brush = CreateSolidBrush(COLORREF(0x001E1E1E));
        let card_brush = CreateSolidBrush(COLORREF(0x00262626));

        // 1. Header Title (fixed on top of flyout window)
        let title_ctrl = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            w!("STATIC"),
            w!("AUDIO ROUTER - QUẢN LÝ ĐẦU RA ỨNG DỤNG"),
            WS_CHILD | WS_VISIBLE | SS_LEFT,
            pad,
            pad,
            width - pad * 2,
            title_h,
            hwnd,
            HMENU(100 as _),
            instance,
            None,
        );
        if let Ok(th) = title_ctrl {
            let _ = SendMessageW(th, WM_SETFONT, WPARAM(title_font.0 as usize), LPARAM(1));
        }

        // 2. Child container window holding the scrollable content
        let container_style = if max_scroll_y > 0 {
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS | WS_VSCROLL
        } else {
            WS_CHILD | WS_VISIBLE | WS_CLIPCHILDREN | WS_CLIPSIBLINGS
        };

        let container_ctrl = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
            container_class,
            w!("ScrollContainer"),
            container_style,
            0,
            container_y,
            width,
            container_h,
            hwnd,
            HMENU(999 as _),
            instance,
            None,
        );

        let container_hwnd = match container_ctrl {
            Ok(c) => c,
            Err(_) => {
                let _ = DestroyWindow(hwnd);
                return;
            }
        };

        if max_scroll_y > 0 {
            let si = SCROLLINFO {
                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                fMask: SIF_RANGE | SIF_PAGE | SIF_POS,
                nMin: 0,
                nMax: content_height,
                nPage: container_h as u32,
                nPos: 0,
                ..Default::default()
            };
            let _ = SetScrollInfo(container_hwnd, SB_VERT, &si, BOOL(1));
        }

        let mut scroll_children = Vec::new();
        let mut app_bindings = Vec::new();

        if apps.is_empty() {
            let empty_x = pad;
            let empty_w = width - pad * 2;
            let empty_h = (48.0 * scale) as i32;
            let empty_ctrl = CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                w!("STATIC"),
                w!("Chưa có ứng dụng nào đang phát âm thanh.\nHãy mở nhạc, video hoặc game rồi mở lại bảng điều khiển."),
                WS_CHILD | WS_VISIBLE | SS_CENTER,
                empty_x,
                (20.0 * scale) as i32,
                empty_w,
                empty_h,
                container_hwnd,
                HMENU::default(),
                instance,
                None,
            );
            if let Ok(eh) = empty_ctrl {
                let _ = SendMessageW(eh, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: eh,
                    x: empty_x,
                    base_y: (20.0 * scale) as i32,
                    w: empty_w,
                    h: empty_h,
                });
            }
        } else {
            for (i, app) in apps.iter().enumerate() {
                let card_y = i as i32 * (card_h + card_gap);
                let base_id = 2000 + i * 20;

                // DÒNG 1: Icon ứng dụng (32x32) + Tên ứng dụng (14pt Bold) + PID (11pt xám)
                let icon = load_app_icon(app.exe_path.as_deref());
                let icon_size = (32.0 * scale) as i32;
                let icon_x = pad + inner_pad;
                let icon_y = card_y + inner_pad;

                if let Some(hicon) = icon {
                    let icon_ctrl = CreateWindowExW(
                        windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                        w!("STATIC"),
                        PCWSTR::null(),
                        WS_CHILD | WS_VISIBLE | SS_ICON,
                        icon_x,
                        icon_y,
                        icon_size,
                        icon_size,
                        container_hwnd,
                        HMENU((base_id + 0) as _),
                        instance,
                        None,
                    );
                    if let Ok(ih) = icon_ctrl {
                        let _ = SendMessageW(ih, STM_SETICON, WPARAM(hicon.0 as usize), LPARAM(0));
                        scroll_children.push(ChildLayout {
                            hwnd: ih,
                            x: icon_x,
                            base_y: icon_y,
                            w: icon_size,
                            h: icon_size,
                        });
                    }
                }

                let name_x = icon_x + icon_size + (10.0 * scale) as i32;
                let pid_w = (110.0 * scale) as i32;
                let pid_x = pad + card_w - inner_pad - pid_w;
                let name_w = pid_x - name_x - (6.0 * scale) as i32;
                let name_h = (26.0 * scale) as i32;
                let name_y = card_y + inner_pad + (3.0 * scale) as i32;

                // App Name Label (14pt Bold)
                let name_wide: Vec<u16> = app.name.encode_utf16().chain(std::iter::once(0)).collect();
                let name_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(name_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_LEFT,
                    name_x,
                    name_y,
                    name_w,
                    name_h,
                    container_hwnd,
                    HMENU((base_id + 1) as _),
                    instance,
                    None,
                );
                let name_hwnd = name_ctrl.unwrap_or_default();
                let _ = SendMessageW(name_hwnd, WM_SETFONT, WPARAM(app_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: name_hwnd,
                    x: name_x,
                    base_y: name_y,
                    w: name_w,
                    h: name_h,
                });

                // App PID Label (11pt xám)
                let pid_display = format!("(PID: {})", app.pid);
                let pid_wide: Vec<u16> = pid_display.encode_utf16().chain(std::iter::once(0)).collect();
                let pid_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(pid_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_RIGHT,
                    pid_x,
                    name_y + (2.0 * scale) as i32,
                    pid_w,
                    (20.0 * scale) as i32,
                    container_hwnd,
                    HMENU((base_id + 2) as _),
                    instance,
                    None,
                );
                let pid_hwnd = pid_ctrl.unwrap_or_default();
                let _ = SendMessageW(pid_hwnd, WM_SETFONT, WPARAM(pid_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: pid_hwnd,
                    x: pid_x,
                    base_y: name_y + (2.0 * scale) as i32,
                    w: pid_w,
                    h: (20.0 * scale) as i32,
                });

                // DÒNG 2: Nhãn Đầu ra 1: (12pt Bold) + ComboBox chọn thiết bị đầu ra chính
                let line2_y = card_y + inner_pad + icon_size + (10.0 * scale) as i32;
                let lbl1_x = pad + inner_pad;
                let lbl1_w = (82.0 * scale) as i32;
                let lbl1_h = (26.0 * scale) as i32;
                let lbl1_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!("Đầu ra 1:"),
                    WS_CHILD | WS_VISIBLE | SS_LEFT,
                    lbl1_x,
                    line2_y + 2,
                    lbl1_w,
                    lbl1_h,
                    container_hwnd,
                    HMENU((base_id + 3) as _),
                    instance,
                    None,
                );
                let lbl1_hwnd = lbl1_ctrl.unwrap_or_default();
                let _ = SendMessageW(lbl1_hwnd, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: lbl1_hwnd,
                    x: lbl1_x,
                    base_y: line2_y + 2,
                    w: lbl1_w,
                    h: lbl1_h,
                });

                let combo1_x = lbl1_x + lbl1_w + (6.0 * scale) as i32;
                let combo1_w = pad + card_w - inner_pad - combo1_x;
                let combo_h = (28.0 * scale) as i32;

                let combo1_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("COMBOBOX"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                    combo1_x,
                    line2_y,
                    combo1_w,
                    (220.0 * scale) as i32,
                    container_hwnd,
                    HMENU((base_id + 4) as _),
                    instance,
                    None,
                );
                let combo1_hwnd = combo1_ctrl.unwrap_or_default();
                let _ = SendMessageW(combo1_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));
                let _ = SetWindowSubclass(combo1_hwnd, Some(no_wheel_subclass), (base_id + 4) as usize, 0);

                scroll_children.push(ChildLayout {
                    hwnd: combo1_hwnd,
                    x: combo1_x,
                    base_y: line2_y,
                    w: combo1_w,
                    h: combo_h,
                });

                // Item 0: Default output
                let def_str = w!("🔊 Mặc định hệ thống");
                let _ = SendMessageW(combo1_hwnd, CB_ADDSTRING, WPARAM(0), LPARAM(def_str.as_ptr() as _));

                let mut sel1 = 0;
                for (dev_idx, dev) in devices.iter().enumerate() {
                    let dev_wide: Vec<u16> = dev.name.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SendMessageW(
                        combo1_hwnd,
                        CB_ADDSTRING,
                        WPARAM(0),
                        LPARAM(dev_wide.as_ptr() as _),
                    );

                    if !app.primary_device_id.is_empty() {
                        if app.primary_device_id == dev.id {
                            sel1 = dev_idx + 1;
                        }
                    } else if dev.id == app.current_device_id {
                        sel1 = dev_idx + 1;
                    }
                }
                let _ = SendMessageW(combo1_hwnd, CB_SETCURSEL, WPARAM(sel1), LPARAM(0));

                // DÒNG 3: Nhãn Đầu ra 2: (12pt Bold) + ComboBox chọn thiết bị đầu ra phụ + Nút toggle [Bật] / [Tắt]
                let line3_y = line2_y + combo_h + (8.0 * scale) as i32;
                let lbl2_x = pad + inner_pad;
                let lbl2_w = (82.0 * scale) as i32;
                let lbl2_h = (26.0 * scale) as i32;
                let lbl2_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    w!("Đầu ra 2:"),
                    WS_CHILD | WS_VISIBLE | SS_LEFT,
                    lbl2_x,
                    line3_y + 2,
                    lbl2_w,
                    lbl2_h,
                    container_hwnd,
                    HMENU((base_id + 5) as _),
                    instance,
                    None,
                );
                let lbl2_hwnd = lbl2_ctrl.unwrap_or_default();
                let _ = SendMessageW(lbl2_hwnd, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: lbl2_hwnd,
                    x: lbl2_x,
                    base_y: line3_y + 2,
                    w: lbl2_w,
                    h: lbl2_h,
                });

                let toggle2_w = (64.0 * scale) as i32;
                let toggle2_h = (26.0 * scale) as i32;
                let toggle2_x = pad + card_w - inner_pad - toggle2_w;
                let combo2_x = lbl2_x + lbl2_w + (6.0 * scale) as i32;
                let combo2_w = toggle2_x - combo2_x - (8.0 * scale) as i32;

                let combo2_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("COMBOBOX"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WS_VSCROLL | WINDOW_STYLE(CBS_DROPDOWNLIST as u32),
                    combo2_x,
                    line3_y,
                    combo2_w,
                    (220.0 * scale) as i32,
                    container_hwnd,
                    HMENU((base_id + 6) as _),
                    instance,
                    None,
                );
                let combo2_hwnd = combo2_ctrl.unwrap_or_default();
                let _ = SendMessageW(combo2_hwnd, WM_SETFONT, WPARAM(small_font.0 as usize), LPARAM(1));
                let _ = SetWindowSubclass(combo2_hwnd, Some(no_wheel_subclass), (base_id + 6) as usize, 0);

                scroll_children.push(ChildLayout {
                    hwnd: combo2_hwnd,
                    x: combo2_x,
                    base_y: line3_y,
                    w: combo2_w,
                    h: combo_h,
                });

                // Item 0: Not selected
                let none_str = w!("-- Chưa chọn --");
                let _ = SendMessageW(combo2_hwnd, CB_ADDSTRING, WPARAM(0), LPARAM(none_str.as_ptr() as _));

                let mut sel2 = 0;
                for (dev_idx, dev) in devices.iter().enumerate() {
                    let dev_wide: Vec<u16> = dev.name.encode_utf16().chain(std::iter::once(0)).collect();
                    let _ = SendMessageW(
                        combo2_hwnd,
                        CB_ADDSTRING,
                        WPARAM(0),
                        LPARAM(dev_wide.as_ptr() as _),
                    );

                    if !app.secondary_device_id.is_empty() && app.secondary_device_id == dev.id {
                        sel2 = dev_idx + 1;
                    }
                }
                let _ = SendMessageW(combo2_hwnd, CB_SETCURSEL, WPARAM(sel2), LPARAM(0));

                // OwnerDraw Toggle Button [Bật] / [Tắt]
                let toggle2_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_OWNERDRAW as u32),
                    toggle2_x,
                    line3_y,
                    toggle2_w,
                    toggle2_h,
                    container_hwnd,
                    HMENU((base_id + 7) as _),
                    instance,
                    None,
                );
                let toggle2_hwnd = toggle2_ctrl.unwrap_or_default();
                let _ = SendMessageW(toggle2_hwnd, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: toggle2_hwnd,
                    x: toggle2_x,
                    base_y: line3_y,
                    w: toggle2_w,
                    h: toggle2_h,
                });

                // DÒNG 4: Nút Mute [Tắt] / [Đã tắt] + Thanh trượt âm lượng Trackbar + Nhãn phần trăm
                let line4_y = line3_y + combo_h + (8.0 * scale) as i32;
                let mute_w = (64.0 * scale) as i32;
                let mute_h = (26.0 * scale) as i32;
                let mute_x = pad + inner_pad;

                let mute_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("BUTTON"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(BS_OWNERDRAW as u32),
                    mute_x,
                    line4_y,
                    mute_w,
                    mute_h,
                    container_hwnd,
                    HMENU((base_id + 8) as _),
                    instance,
                    None,
                );
                let mute_hwnd = mute_ctrl.unwrap_or_default();
                let _ = SendMessageW(mute_hwnd, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: mute_hwnd,
                    x: mute_x,
                    base_y: line4_y,
                    w: mute_w,
                    h: mute_h,
                });

                let pct_w = (46.0 * scale) as i32;
                let pct_x = pad + card_w - inner_pad - pct_w;
                let slider_x = mute_x + mute_w + (8.0 * scale) as i32;
                let slider_w = pct_x - slider_x - (6.0 * scale) as i32;
                let slider_h = (26.0 * scale) as i32;

                let slider_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("msctls_trackbar32"),
                    PCWSTR::null(),
                    WS_CHILD | WS_VISIBLE | WINDOW_STYLE(TBS_HORZ as u32 | TBS_AUTOTICKS as u32),
                    slider_x,
                    line4_y,
                    slider_w,
                    slider_h,
                    container_hwnd,
                    HMENU((base_id + 9) as _),
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
                let _ = SendMessageW(slider_hwnd, TBM_SETPAGESIZE, WPARAM(0), LPARAM(5));
                let _ = SendMessageW(slider_hwnd, TBM_SETLINESIZE, WPARAM(0), LPARAM(1));
                let vol_val = (app.volume * 100.0).clamp(0.0, 100.0) as u32;
                let _ = SendMessageW(slider_hwnd, TBM_SETPOS, WPARAM(1), LPARAM(vol_val as isize));
                let _ = SetWindowSubclass(slider_hwnd, Some(no_wheel_subclass), (base_id + 9) as usize, 0);

                scroll_children.push(ChildLayout {
                    hwnd: slider_hwnd,
                    x: slider_x,
                    base_y: line4_y,
                    w: slider_w,
                    h: slider_h,
                });

                let pct_str = format!("{}%", vol_val);
                let pct_wide: Vec<u16> = pct_str.encode_utf16().chain(std::iter::once(0)).collect();
                let pct_ctrl = CreateWindowExW(
                    windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    PCWSTR(pct_wide.as_ptr()),
                    WS_CHILD | WS_VISIBLE | SS_CENTER,
                    pct_x,
                    line4_y + 2,
                    pct_w,
                    (22.0 * scale) as i32,
                    container_hwnd,
                    HMENU((base_id + 10) as _),
                    instance,
                    None,
                );
                let percent_hwnd = pct_ctrl.unwrap_or_default();
                let _ = SendMessageW(percent_hwnd, WM_SETFONT, WPARAM(label_font.0 as usize), LPARAM(1));
                scroll_children.push(ChildLayout {
                    hwnd: percent_hwnd,
                    x: pct_x,
                    base_y: line4_y + 2,
                    w: pct_w,
                    h: (22.0 * scale) as i32,
                });

                app_bindings.push(AppControlBinding {
                    app_index: i,
                    combo1_hwnd,
                    combo2_hwnd,
                    toggle2_hwnd,
                    mute_hwnd,
                    slider_hwnd,
                    percent_hwnd,
                    icon,
                });
            }
        }

        let state = Box::new(FlyoutState {
            owner_hwnd,
            container_hwnd,
            scale,
            scroll_y: 0,
            max_scroll_y,
            container_h,
            card_h,
            card_gap,
            scroll_children,
            devices,
            apps,
            bg_brush,
            card_brush,
            title_font,
            app_font,
            label_font,
            small_font,
            pid_font,
            is_dropdown_open: false,
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

        WM_MOUSEWHEEL => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.max_scroll_y > 0 {
                    let delta = ((wparam.0 as i32) >> 16) as i16 as i32;
                    let notches = delta / 120;
                    let step = if notches != 0 {
                        notches * (50.0 * state.scale) as i32
                    } else {
                        (delta * (50.0 * state.scale) as i32) / 120
                    };
                    let new_scroll_y = (state.scroll_y - step).clamp(0, state.max_scroll_y);
                    if new_scroll_y != state.scroll_y {
                        state.scroll_y = new_scroll_y;
                        state.apply_scroll();
                    }
                    return LRESULT(0);
                }
            }
        }

        WM_VSCROLL => {
            if !state_ptr.is_null() {
                let state = &mut *state_ptr;
                if state.max_scroll_y > 0 {
                    let action = (wparam.0 as u32) & 0xFFFF;
                    let old_pos = state.scroll_y;
                    let line_step = (30.0 * state.scale) as i32;
                    let page_step = state.container_h;
                    let new_pos = match action {
                        x if x == SB_LINEUP.0 as u32 => (old_pos - line_step).max(0),
                        x if x == SB_LINEDOWN.0 as u32 => (old_pos + line_step).min(state.max_scroll_y),
                        x if x == SB_PAGEUP.0 as u32 => (old_pos - page_step).max(0),
                        x if x == SB_PAGEDOWN.0 as u32 => (old_pos + page_step).min(state.max_scroll_y),
                        x if x == SB_THUMBTRACK.0 as u32 || x == SB_THUMBPOSITION.0 as u32 => {
                            let mut si = SCROLLINFO {
                                cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
                                fMask: SIF_ALL,
                                ..Default::default()
                            };
                            let _ = GetScrollInfo(state.container_hwnd, SB_VERT, &mut si);
                            si.nTrackPos.clamp(0, state.max_scroll_y)
                        }
                        _ => old_pos,
                    };
                    if new_pos != old_pos {
                        state.scroll_y = new_pos;
                        state.apply_scroll();
                    }
                    return LRESULT(0);
                }
            }
        }

        WM_DRAWITEM => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let dis = &*(lparam.0 as *const DRAWITEMSTRUCT);
                let btn_hwnd = dis.hwndItem;
                let is_selected = (dis.itemState.0 & ODS_SELECTED.0) != 0;

                // 1. Check if it's a Secondary Output Toggle Button
                for b in &state.app_bindings {
                    if b.toggle2_hwnd == btn_hwnd {
                        let app = &state.apps[b.app_index];
                        let (bg_col, border_col, text_col, btn_text) = if app.secondary_enabled {
                            // Dark Green #2E7D32, border #388E3C, text white
                            let bg = if is_selected { COLORREF(0x00246326) } else { COLORREF(0x00327D2E) };
                            let border = COLORREF(0x003C8E38);
                            let text = COLORREF(0x00FFFFFF);
                            (bg, border, text, "Bật")
                        } else {
                            // Dark Gray #383838, border #4E4E4E, text gray
                            let bg = if is_selected { COLORREF(0x00484848) } else { COLORREF(0x00383838) };
                            let border = COLORREF(0x004E4E4E);
                            let text = COLORREF(0x00CCCCCC);
                            (bg, border, text, "Tắt")
                        };

                        let pen = CreatePen(PS_SOLID, 1, border_col);
                        let brush = CreateSolidBrush(bg_col);
                        let old_pen = SelectObject(dis.hDC, pen);
                        let old_brush = SelectObject(dis.hDC, brush);

                        let corner = (6.0 * state.scale) as i32;
                        let _ = RoundRect(
                            dis.hDC,
                            dis.rcItem.left,
                            dis.rcItem.top,
                            dis.rcItem.right,
                            dis.rcItem.bottom,
                            corner,
                            corner,
                        );

                        let _ = SelectObject(dis.hDC, old_pen);
                        let _ = SelectObject(dis.hDC, old_brush);
                        let _ = DeleteObject(pen);
                        let _ = DeleteObject(brush);

                        let _ = SetBkMode(dis.hDC, TRANSPARENT);
                        let _ = SetTextColor(dis.hDC, text_col);
                        let old_font = SelectObject(dis.hDC, state.label_font);

                        let mut wide_text: Vec<u16> = btn_text.encode_utf16().chain(std::iter::once(0)).collect();
                        let mut rc = dis.rcItem;
                        let _ = DrawTextW(
                            dis.hDC,
                            &mut wide_text[..btn_text.encode_utf16().count()],
                            &mut rc,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );

                        let _ = SelectObject(dis.hDC, old_font);
                        return LRESULT(1);
                    }
                }

                // 2. Check if it's an App Mute Button
                for b in &state.app_bindings {
                    if b.mute_hwnd == btn_hwnd {
                        let app = &state.apps[b.app_index];
                        let (bg_col, border_col, text_col, btn_text) = if app.is_muted {
                            // Warm dark red #5A1E1E (border #8A2E2E), soft red text #FF9999 "Đã tắt"
                            let bg = if is_selected { COLORREF(0x0024246E) } else { COLORREF(0x001E1E5A) };
                            let border = COLORREF(0x002E2E8A);
                            let text = COLORREF(0x009999FF);
                            (bg, border, text, "Đã tắt")
                        } else {
                            // Dark Gray #383838, border #4E4E4E, text white "Tắt"
                            let bg = if is_selected { COLORREF(0x00484848) } else { COLORREF(0x00383838) };
                            let border = COLORREF(0x004E4E4E);
                            let text = COLORREF(0x00FFFFFF);
                            (bg, border, text, "Tắt")
                        };

                        let pen = CreatePen(PS_SOLID, 1, border_col);
                        let brush = CreateSolidBrush(bg_col);
                        let old_pen = SelectObject(dis.hDC, pen);
                        let old_brush = SelectObject(dis.hDC, brush);

                        let corner = (6.0 * state.scale) as i32;
                        let _ = RoundRect(
                            dis.hDC,
                            dis.rcItem.left,
                            dis.rcItem.top,
                            dis.rcItem.right,
                            dis.rcItem.bottom,
                            corner,
                            corner,
                        );

                        let _ = SelectObject(dis.hDC, old_pen);
                        let _ = SelectObject(dis.hDC, old_brush);
                        let _ = DeleteObject(pen);
                        let _ = DeleteObject(brush);

                        let _ = SetBkMode(dis.hDC, TRANSPARENT);
                        let _ = SetTextColor(dis.hDC, text_col);
                        let old_font = SelectObject(dis.hDC, state.label_font);

                        let mut wide_text: Vec<u16> = btn_text.encode_utf16().chain(std::iter::once(0)).collect();
                        let mut rc = dis.rcItem;
                        let _ = DrawTextW(
                            dis.hDC,
                            &mut wide_text[..btn_text.encode_utf16().count()],
                            &mut rc,
                            DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                        );

                        let _ = SelectObject(dis.hDC, old_font);
                        return LRESULT(1);
                    }
                }
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
                    for b in &state.app_bindings {
                        // ComboBox 1 (Đầu ra 1 - Primary Output)
                        if b.combo1_hwnd.0 as isize == lparam.0 {
                            let sel = SendMessageW(b.combo1_hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                            let app = &mut state.apps[b.app_index];
                            let mut config = load_config_v3();

                            if sel == 0 {
                                // Default Windows output
                                app.primary_device_id.clear();
                                app.current_device_id.clear();
                                config.set_app_primary_device(app.name.clone(), String::new());
                                let _ = save_config_v3(&config);

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
                                let dev = &state.devices[sel - 1];
                                app.primary_device_id = dev.id.clone();
                                app.current_device_id = dev.id.clone();
                                config.set_app_primary_device(app.name.clone(), dev.id.clone());
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

                        // ComboBox 2 (Đầu ra 2 - Secondary Output)
                        if b.combo2_hwnd.0 as isize == lparam.0 {
                            let sel = SendMessageW(b.combo2_hwnd, CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as usize;
                            let app = &mut state.apps[b.app_index];
                            let mut config = load_config_v3();

                            if sel == 0 {
                                app.secondary_device_id.clear();
                                config.set_app_secondary_device(app.name.clone(), String::new());
                                let _ = save_config_v3(&config);
                            } else if sel - 1 < state.devices.len() {
                                let dev = &state.devices[sel - 1];
                                app.secondary_device_id = dev.id.clone();
                                config.set_app_secondary_device(app.name.clone(), dev.id.clone());
                                let _ = save_config_v3(&config);
                            }

                            if app.secondary_enabled {
                                let _ = PostMessageW(
                                    state.owner_hwnd,
                                    WM_APP_APPLY_RULES,
                                    WPARAM(app.pid as usize),
                                    LPARAM(2),
                                );
                            }
                            break;
                        }
                    }
                } else if notify_code == BN_CLICKED as u32 {
                    // Check Secondary Output Toggle Button
                    for b in &mut state.app_bindings {
                        if b.toggle2_hwnd.0 as isize == lparam.0 {
                            let app = &mut state.apps[b.app_index];
                            app.secondary_enabled = !app.secondary_enabled;

                            // If enabling secondary output but none was selected, auto-select first available other device
                            if app.secondary_enabled && app.secondary_device_id.is_empty() {
                                let prim_id = if !app.primary_device_id.is_empty() {
                                    &app.primary_device_id
                                } else {
                                    &app.current_device_id
                                };
                                if let Some((idx, dev)) = state.devices.iter().enumerate().find(|(_, d)| &d.id != prim_id) {
                                    app.secondary_device_id = dev.id.clone();
                                    let _ = SendMessageW(b.combo2_hwnd, CB_SETCURSEL, WPARAM(idx + 1), LPARAM(0));
                                    let mut config = load_config_v3();
                                    config.set_app_secondary_device(app.name.clone(), dev.id.clone());
                                    let _ = save_config_v3(&config);
                                }
                            }

                            let _ = InvalidateRect(b.toggle2_hwnd, None, BOOL(1));

                            let mut config = load_config_v3();
                            config.set_app_secondary_enabled(app.name.clone(), app.secondary_enabled);
                            let _ = save_config_v3(&config);

                            let _ = PostMessageW(
                                state.owner_hwnd,
                                WM_APP_APPLY_RULES,
                                WPARAM(app.pid as usize),
                                LPARAM(if app.secondary_enabled { 2 } else { 0 }),
                            );
                            return LRESULT(0);
                        }
                    }

                    // Check App Mute Button
                    for b in &mut state.app_bindings {
                        if b.mute_hwnd.0 as isize == lparam.0 {
                            let app = &mut state.apps[b.app_index];
                            app.is_muted = !app.is_muted;
                            let _ = InvalidateRect(b.mute_hwnd, None, BOOL(1));

                            let _ = crate::audio::set_app_mute(app.pid, &app.current_device_id, app.is_muted);

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

                for b in &mut state.app_bindings {
                    if b.slider_hwnd == slider_hwnd {
                        let pos = SendMessageW(slider_hwnd, TBM_GETPOS, WPARAM(0), LPARAM(0)).0 as u32;
                        let app = &mut state.apps[b.app_index];
                        let new_vol = pos as f32 / 100.0;
                        app.volume = new_vol;

                        let text = format!("{}%", pos);
                        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
                        let _ = SetWindowTextW(b.percent_hwnd, PCWSTR(wide.as_ptr()));

                        let _ = crate::audio::set_app_volume(app.pid, &app.current_device_id, new_vol);

                        let mut config = load_config_v3();
                        config.set_app_volume(app.name.clone(), new_vol);
                        let _ = save_config_v3(&config);
                        return LRESULT(0);
                    }
                }
            }
        }

        WM_CTLCOLORSTATIC | WM_CTLCOLORBTN | WM_CTLCOLORDLG => {
            if !state_ptr.is_null() {
                let state = &*state_ptr;
                let hdc = HDC(wparam.0 as _);
                let ctrl_id = GetDlgCtrlID(HWND(lparam.0 as _));

                if ctrl_id == 100 {
                    // Header title
                    let _ = SetTextColor(hdc, COLORREF(0x00FFCD60));
                    let _ = SetBkColor(hdc, COLORREF(0x001E1E1E));
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    return LRESULT(state.bg_brush.0 as isize);
                }

                // Per-card controls
                if ctrl_id >= 2000 {
                    let sub_id = ctrl_id % 20;
                    if sub_id == 1 {
                        // App Name (14pt Bold White)
                        let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF));
                    } else if sub_id == 2 {
                        // PID (11pt Gray #9E9E9E -> COLORREF 0x009E9E9E)
                        let _ = SetTextColor(hdc, COLORREF(0x009E9E9E));
                    } else if sub_id == 3 || sub_id == 5 {
                        // Labels "Đầu ra 1:" / "Đầu ra 2:" (12pt Bold Light Gray #E0E0E0)
                        let _ = SetTextColor(hdc, COLORREF(0x00E0E0E0));
                    } else if sub_id == 10 {
                        // Volume percent label
                        let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF));
                    } else {
                        let _ = SetTextColor(hdc, COLORREF(0x00CCCCCC));
                    }

                    let _ = SetBkColor(hdc, COLORREF(0x00262626));
                    let _ = SetBkMode(hdc, TRANSPARENT);
                    return LRESULT(state.card_brush.0 as isize);
                }

                let _ = SetTextColor(hdc, COLORREF(0x00FFFFFF));
                let _ = SetBkColor(hdc, COLORREF(0x001E1E1E));
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
                let _ = DeleteObject(state.label_font);
                let _ = DeleteObject(state.small_font);
                let _ = DeleteObject(state.pid_font);

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
    fn test_app_audio_item_creation() {
        let app = AppAudioItem::new(1234, "brave.exe".into());
        assert_eq!(app.pid, 1234);
        assert_eq!(app.name, "brave.exe");
        assert_eq!(app.primary_device_id, "");
        assert_eq!(app.secondary_device_id, "");
        assert!(!app.secondary_enabled);
        assert_eq!(app.volume, 1.0);
        assert!(!app.is_muted);
    }

    #[test]
    fn test_calculate_flyout_pos() {
        let (x, y) = calculate_flyout_position(520, 360, None);
        assert!(x >= 0);
        assert!(y >= 0);

        let dummy_rect = RECT {
            left: 1700,
            top: 1040,
            right: 1740,
            bottom: 1080,
        };
        let (x2, y2) = calculate_flyout_position(520, 360, Some(dummy_rect));
        assert!(x2 > 0);
        assert!(y2 > 0);
    }
}
