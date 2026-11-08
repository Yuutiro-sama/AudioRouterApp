pub mod app_state;
pub mod audio;
pub mod config;
pub mod ipc_messages;
pub mod ui;

pub fn debug_log(msg: &str) {
    if let Ok(dir) = std::env::var("APPDATA") {
        let p = format!("{}\\AudioRouter\\debug.log", dir);
        let _ = std::fs::OpenOptions::new().create(true).append(true).open(p).and_then(|mut f| {
            use std::io::Write;
            writeln!(f, "[{}] {}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis(), msg)
        });
    }
}

