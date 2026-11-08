fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() == "windows" {
        let mut res = winres::WindowsResource::new();
        res.set_icon("app.ico");
        res.set("ProductName", "Audio Router");
        res.set("FileDescription", "Audio Router - Real-Time Multi-Endpoint Audio Routing");
        res.set("FileVersion", "3.0.0.0");
        res.set("ProductVersion", "3.0.0.0");
        if let Err(e) = res.compile() {
            eprintln!("winres compile warning: {:?}", e);
        }
    }
}
