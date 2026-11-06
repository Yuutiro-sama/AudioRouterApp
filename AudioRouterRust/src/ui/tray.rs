use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW,
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconGetRect, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP,
    NIM_ADD, NIM_DELETE, NIM_MODIFY, NIM_SETVERSION, NOTIFYICONDATAW, NOTIFYICONIDENTIFIER,
    NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu,
    GetCursorPos, PostMessageW, RegisterWindowMessageW, SetForegroundWindow, TrackPopupMenu,
    HICON, ICONINFO, MF_CHECKED, MF_SEPARATOR, MF_STRING, MF_UNCHECKED, TPM_BOTTOMALIGN,
    TPM_RIGHTBUTTON, WM_NULL,
};

use crate::ipc_messages::{IDM_APPLY_RULES, IDM_EXIT, IDM_STARTUP};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("AudioRouter");

/// Registers the Explorer TaskbarCreated broadcast message
pub fn register_taskbar_created_message() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// Checks if AudioRouter is registered in HKCU Run key
pub fn is_run_at_startup() -> bool {
    unsafe {
        let mut hkey = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_READ, &mut hkey).is_ok() {
            let res = RegQueryValueExW(hkey, VALUE_NAME, None, None, None, None);
            let _ = RegCloseKey(hkey);
            res.is_ok()
        } else {
            false
        }
    }
}

/// Sets or deletes the HKCU Run key for AudioRouter
pub fn set_run_at_startup(enable: bool) -> Result<(), windows::core::Error> {
    unsafe {
        let mut hkey = HKEY::default();
        RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, KEY_WRITE, &mut hkey).ok()?;

        let result = if enable {
            if let Ok(exe_path) = std::env::current_exe() {
                let path_str = format!("\"{}\"", exe_path.display());
                let wide: Vec<u16> = path_str.encode_utf16().chain(std::iter::once(0)).collect();
                let bytes = std::slice::from_raw_parts(
                    wide.as_ptr() as *const u8,
                    wide.len() * std::mem::size_of::<u16>(),
                );
                RegSetValueExW(hkey, VALUE_NAME, 0, REG_SZ, Some(bytes)).ok()
            } else {
                windows::Win32::Foundation::WIN32_ERROR(1).ok()
            }
        } else {
            let res = RegDeleteValueW(hkey, VALUE_NAME);
            if res.is_ok() || res == windows::Win32::Foundation::WIN32_ERROR(2).into() {
                Ok(())
            } else {
                res.ok()
            }
        };

        let _ = RegCloseKey(hkey);
        result
    }
}

/// Creates a vector speaker icon matching the C# design (Blue circle background, white speaker, sound waves)
pub fn create_speaker_icon() -> Result<HICON, windows::core::Error> {
    const WIDTH: usize = 32;
    const HEIGHT: usize = 32;
    let mut pixels = [0u32; WIDTH * HEIGHT];

    // Background circle: #0078D7 (R: 0, G: 120, B: 215)
    let center_x = 15.5f32;
    let center_y = 15.5f32;
    let radius = 14.5f32;

    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            let dx = x as f32 - center_x;
            let dy = y as f32 - center_y;
            let dist = (dx * dx + dy * dy).sqrt();

            if dist <= radius {
                let alpha = if dist > radius - 1.0 {
                    ((radius - dist) * 255.0).clamp(0.0, 255.0) as u32
                } else {
                    255u32
                };

                let r = (0 * alpha) / 255;
                let g = (120 * alpha) / 255;
                let b = (215 * alpha) / 255;
                pixels[y * WIDTH + x] = (alpha << 24) | (r << 16) | (g << 8) | b;
            }
        }
    }

    // 1. Speaker body: rectangle x in 7..11, y in 12..20
    for y in 12..20 {
        for x in 7..11 {
            pixels[y * WIDTH + x] = 0xFFFFFFFF;
        }
    }

    // 2. Speaker cone: trapezoid/polygon (11, 12), (17, 7), (17, 25), (11, 20)
    for y in 7..=24 {
        for x in 11..=17 {
            let top_y = 12.0 - (5.0 / 6.0) * (x as f32 - 11.0);
            let bot_y = 20.0 + (5.0 / 6.0) * (x as f32 - 11.0);

            if (y as f32) >= top_y && (y as f32) <= bot_y {
                pixels[y * WIDTH + x] = 0xFFFFFFFF;
            }
        }
    }

    // 3. Sound waves: arcs around (14, 15.5)
    for y in 6..=26 {
        for x in 15..=27 {
            let dx = x as f32 - 14.0;
            let dy = y as f32 - 15.5;
            let dist = (dx * dx + dy * dy).sqrt();
            let angle_deg = (dy / dx).atan().to_degrees();

            if angle_deg >= -52.0 && angle_deg <= 52.0 {
                // Wave 1: dist ~ 5.0 to 7.0
                if dist >= 5.0 && dist <= 7.0 {
                    pixels[y * WIDTH + x] = 0xFFFFFFFF;
                }
                // Wave 2: dist ~ 9.0 to 11.0
                if dist >= 9.0 && dist <= 11.0 {
                    pixels[y * WIDTH + x] = 0xFFFFFFFF;
                }
            }
        }
    }

    unsafe {
        let hbm_color = CreateBitmap(
            WIDTH as i32,
            HEIGHT as i32,
            1,
            32,
            Some(pixels.as_ptr() as *const _),
        );

        let mask_bytes = [0u8; (WIDTH * HEIGHT) / 8];
        let hbm_mask = CreateBitmap(
            WIDTH as i32,
            HEIGHT as i32,
            1,
            1,
            Some(mask_bytes.as_ptr() as *const _),
        );

        let icon_info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: hbm_mask,
            hbmColor: hbm_color,
        };

        let hicon = CreateIconIndirect(&icon_info)?;

        let _ = DeleteObject(hbm_color);
        let _ = DeleteObject(hbm_mask);

        Ok(hicon)
    }
}

/// System Tray Icon controller
pub struct TrayIcon {
    hwnd: HWND,
    nid: NOTIFYICONDATAW,
    icon: HICON,
    is_added: bool,
}

impl TrayIcon {
    pub const TRAY_ICON_ID: u32 = 1;

    /// Creates and initializes a new system tray icon
    pub fn new(hwnd: HWND, callback_msg: u32, tooltip: &str) -> Result<Self, windows::core::Error> {
        let icon = create_speaker_icon()?;

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: Self::TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: callback_msg,
            hIcon: icon,
            ..Default::default()
        };

        // Copy tooltip wide chars
        let wide_tip: Vec<u16> = tooltip.encode_utf16().collect();
        let len = wide_tip.len().min(nid.szTip.len() - 1);
        nid.szTip[..len].copy_from_slice(&wide_tip[..len]);
        nid.szTip[len] = 0;

        // Set version 4 for modern Shell_NotifyIcon notifications
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;

        Ok(Self {
            hwnd,
            nid,
            icon,
            is_added: false,
        })
    }

    /// Adds the icon to the notification area
    pub fn add(&mut self) -> Result<(), windows::core::Error> {
        unsafe {
            Shell_NotifyIconW(NIM_ADD, &self.nid).ok()?;
            Shell_NotifyIconW(NIM_SETVERSION, &self.nid).ok()?;
            self.is_added = true;
            Ok(())
        }
    }

    /// Re-registers the icon (e.g. after Explorer restarts upon receiving WM_TASKBARCREATED)
    pub fn recreate(&mut self) -> Result<(), windows::core::Error> {
        self.is_added = false;
        self.add()
    }

    /// Modifies the tooltip text
    pub fn update_tooltip(&mut self, tooltip: &str) -> Result<(), windows::core::Error> {
        let wide_tip: Vec<u16> = tooltip.encode_utf16().collect();
        let len = wide_tip.len().min(self.nid.szTip.len() - 1);
        self.nid.szTip[..len].copy_from_slice(&wide_tip[..len]);
        self.nid.szTip[len] = 0;
        self.nid.uFlags = NIF_TIP;

        unsafe {
            Shell_NotifyIconW(NIM_MODIFY, &self.nid).ok()?;
            // Restore full flags
            self.nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP;
            Ok(())
        }
    }

    /// Removes the icon from the notification area
    pub fn remove(&mut self) -> Result<(), windows::core::Error> {
        if self.is_added {
            unsafe {
                Shell_NotifyIconW(NIM_DELETE, &self.nid).ok()?;
            }
            self.is_added = false;
        }
        Ok(())
    }

    /// Returns the bounding rectangle of the tray icon if supported by the OS
    pub fn get_rect(&self) -> Option<RECT> {
        unsafe {
            let identifier = NOTIFYICONIDENTIFIER {
                cbSize: std::mem::size_of::<NOTIFYICONIDENTIFIER>() as u32,
                hWnd: self.hwnd,
                uID: Self::TRAY_ICON_ID,
                guidItem: windows::core::GUID::zeroed(),
            };
            Shell_NotifyIconGetRect(&identifier).ok()
        }
    }

    /// Displays the right-click context menu
    pub fn show_context_menu(&self) {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);

            // Win32 requirement: SetForegroundWindow must be called before TrackPopupMenu
            let _ = SetForegroundWindow(self.hwnd);

            let hmenu = match CreatePopupMenu() {
                Ok(menu) => menu,
                Err(_) => return,
            };

            // 1. Áp dụng tất cả quy tắc
            let _ = AppendMenuW(
                hmenu,
                MF_STRING,
                IDM_APPLY_RULES,
                w!("Áp dụng tất cả quy tắc"),
            );

            // 2. Khởi động cùng Windows (Checkbox)
            let startup_flags = if is_run_at_startup() {
                MF_STRING | MF_CHECKED
            } else {
                MF_STRING | MF_UNCHECKED
            };
            let _ = AppendMenuW(
                hmenu,
                startup_flags,
                IDM_STARTUP,
                w!("Khởi động cùng Windows"),
            );

            // 3. Separator
            let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());

            // 4. Thoát
            let _ = AppendMenuW(hmenu, MF_STRING, IDM_EXIT, w!("Thoát"));

            // Display menu
            let _ = TrackPopupMenu(
                hmenu,
                TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
                pt.x,
                pt.y,
                0,
                self.hwnd,
                None,
            );

            // Post WM_NULL to force focus cleanup after menu dismisses
            let _ = PostMessageW(self.hwnd, WM_NULL, WPARAM(0), LPARAM(0));
            let _ = DestroyMenu(hmenu);
        }
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        let _ = self.remove();
        if !self.icon.is_invalid() {
            unsafe {
                let _ = DestroyIcon(self.icon);
            }
        }
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_speaker_icon_generation() {
        let icon_res = create_speaker_icon();
        assert!(icon_res.is_ok());
        let icon = icon_res.unwrap();
        assert!(!icon.is_invalid());
        unsafe {
            let _ = DestroyIcon(icon);
        }
    }

    #[test]
    fn test_taskbar_created_registration() {
        let msg = register_taskbar_created_message();
        assert!(msg > 0);
    }
}
