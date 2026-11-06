use windows::Win32::UI::WindowsAndMessaging::WM_APP;

// Custom Win32 Window Messages for Application IPC & Dispatcher
pub const WM_APP_TRAY: u32 = WM_APP + 1;
pub const WM_APP_DEVICE_HOTPLUG: u32 = WM_APP + 2;
pub const WM_APP_DEFAULT_CHANGED: u32 = WM_APP + 3;
pub const WM_APP_APPLY_RULES: u32 = WM_APP + 4;
pub const WM_APP_REFRESH_UI: u32 = WM_APP + 5;
pub const WM_APP_SESSION_CREATED: u32 = WM_APP + 6;

// Tray Context Menu Command IDs
pub const IDM_SHOW_UI: usize = 1000;
pub const IDM_APPLY_RULES: usize = 1001;
pub const IDM_STARTUP: usize = 1002;
pub const IDM_EXIT: usize = 1003;

// Timer Identifiers
pub const TIMER_ID_DEBOUNCE: usize = 101;
pub const DEBOUNCE_DELAY_MS: u32 = 150;
