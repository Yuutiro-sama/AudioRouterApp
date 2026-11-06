# Audio Router App 🎧

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-lightgrey.svg)](https://microsoft.com/windows)
[![Rust](https://img.shields.io/badge/Rust-Native%20Win32%2FCOM-orange.svg)](https://www.rust-lang.org/)
[![Binary Size](https://img.shields.io/badge/Binary-~357%20KB-success.svg)](#-ưu-điểm-nổi-bật)
[![Memory Footprint](https://img.shields.io/badge/RAM-%3C%201.5%20MB-brightgreen.svg)](#-ưu-điểm-nổi-bật)

> [!NOTE]  
> **Tuyên bố về AI và tính thương mại:**  
> Dự án này được phát triển với sự hỗ trợ của các mô hình AI phục vụ mục đích nghiên cứu, học tập và thử nghiệm ý tưởng (PoC). Mã nguồn và tài liệu do AI sinh ra đóng vai trò là khung tham khảo; dự án không được thiết kế sẵn cho mục đích thương mại trực tiếp và chưa được kiểm thử toàn diện cho môi trường vận hành thực tế (production).
>
> **AI and Commercial Disclaimer:**  
> This project was developed with the assistance of AI models for research, educational, and Proof-of-Concept (PoC) purposes. The AI-generated source code and documentation serve as a reference framework; this project is not designed for direct commercial deployment and has not been extensively tested for production environments.

---

[🇻🇳 Tiếng Việt](#-tiếng-việt) | [🇬🇧 English](#-english)

---

## 🇻🇳 Tiếng Việt

**Audio Router App** là tiện ích mã nguồn mở siêu nhẹ dành cho Windows, cho phép định tuyến âm thanh của từng ứng dụng riêng biệt đến các thiết bị phát khác nhau (tai nghe, loa ngoài, USB DAC, thiết bị âm thanh ảo), điều chỉnh âm lượng từng app và tự động khôi phục cấu hình ngay khi cắm hoặc rút thiết bị.

> [!TIP]
> **Phiên bản mới (v2.0) được viết lại hoàn toàn bằng ngôn ngữ Rust thuần (Native Win32/COM)**, giúp giảm dung lượng file thực thi từ 72 MB xuống chỉ còn **~357 KB** và mức tiêu thụ RAM chạy nền chỉ còn **< 1.5 MB**! (Mã nguồn C# .NET 10 ban đầu vẫn được lưu trữ song song trong repo).

### ⚡ Ưu điểm nổi bật

- 🚀 **Siêu nhẹ - File chạy chỉ ~357 KB, tiêu thụ < 1.5 MB RAM**: 
  - Hoàn toàn độc lập, không cần cài đặt .NET Runtime, Visual C++ Redistributable hay bất kỳ phần mềm phụ trợ nào.
  - Khi ẩn ở khay hệ thống (System Tray), app chỉ tốn khoảng **1.0 - 1.5 MB RAM** và **0% CPU**.
- 🎯 **Định tuyến âm thanh độc lập từng app**: Nghe nhạc từ trình duyệt qua tai nghe, phát nhạc game ra loa ngoài hoặc chuyển âm thanh Discord sang tai nghe đàm thoại riêng biệt.
- 🔌 **Tự động nhận diện khi cắm/rút thiết bị (Hardware Hotplug)**:
  - Lắng nghe trực tiếp sự kiện từ Windows Core Audio (`IMMNotificationClient`).
  - Tự động ghi nhớ và khôi phục âm thanh về đúng tai nghe/DAC ngay khi cắm lại mà không cần cấu hình lại bằng tay.
- 🎚️ **Bộ trộn âm lượng (Per-App Volume & Mute)**: Điều chỉnh thanh trượt âm lượng và nút tắt tiếng từng ứng dụng trực tiếp trên bảng điều khiển khay hệ thống.
- 💾 **Tự động ghi nhớ cấu hình**: Lưu quy tắc vào `%AppData%\AudioRouter\rules.json` và tự động áp dụng lại mỗi khi ứng dụng đó phát âm thanh.
- ⏰ **Khởi động cùng Windows**: Tích hợp menu chuột phải bật/tắt tự khởi động cùng máy tính, không đòi hỏi quyền Administrator.

### 🛡️ Bảo mật & Quyền riêng tư

- **100% Ngoại tuyến (Offline)**: Hoàn toàn không kết nối internet, không thu thập dữ liệu (No Telemetry).
- **Nguyên tắc đặc quyền tối thiểu (Least Privilege)**: Chạy hoàn toàn dưới quyền người dùng thông thường (`PROCESS_QUERY_LIMITED_INFORMATION`), không yêu cầu Run as Administrator.
- **An toàn dữ liệu**: Dữ liệu cấu hình chỉ lưu trữ cục bộ trên máy tính cá nhân.

### 📋 Yêu cầu hệ thống

- **Hệ điều hành**: Windows 10 (bản 1903 trở lên) hoặc Windows 11 (64-bit).
- **Phần mềm phụ trợ**: **Không cần cài đặt thêm bất cứ gì** khi tải file `AudioRouter.exe` (Rust release).

### 🛠️ Hướng dẫn build từ mã nguồn

#### 1. Bản Rust Native (Khuyên dùng - 357 KB, < 1.5 MB RAM)
```bash
# Clone repository
git clone https://github.com/Yuutiro-sama/AudioRouterApp.git
cd AudioRouterApp/AudioRouterRust

# Chạy kiểm thử tự động
cargo test

# Build bản Release tối ưu cao nhất
cargo build --release
```
File thực thi độc lập sẽ nằm tại `AudioRouterRust/target/release/AudioRouter.exe`.

#### 2. Bản C# .NET 10 WPF (Bản gốc)
```bash
cd AudioRouterApp
dotnet build
dotnet publish AudioRouterApp.csproj -c Release -r win-x64 --self-contained true -p:PublishSingleFile=true -o ./PublishSingleFile
```

### 🚀 Hướng dẫn sử dụng

1. Tải file `AudioRouter.exe` từ mục [Releases](https://github.com/Yuutiro-sama/AudioRouterApp/releases).
2. Chạy file `AudioRouter.exe`.
3. Biểu tượng loa sẽ xuất hiện ở khay hệ thống (System Tray, góc dưới bên phải màn hình).
4. Nhấp chuột trái vào biểu tượng để mở bảng điều chỉnh thiết bị và âm lượng cho từng ứng dụng.

---

## 🇬🇧 English

**Audio Router App** is a high-performance, ultra-lightweight Windows utility that routes audio from specific applications to specific output devices (headphones, speakers, DACs, virtual audio cables) independently, adjusts per-app volume, and automatically restores routing when devices are connected or disconnected.

> [!TIP]
> **Version 2.0 has been completely rewritten in native Rust (Win32/COM)**, slashing executable size from 72 MB down to just **~357 KB**, and idling background RAM footprint to **< 1.5 MB**! (The original C# .NET 10 source code remains available in this repository).

### ⚡ Key Features

- 🚀 **Ultra-Lightweight (~357 KB Executable, < 1.5 MB Idle RAM)**: Zero external runtimes required (no .NET Runtime or VC++ Redistributable needed). Uses zero CPU when idling in system tray.
- 🎯 **Per-Application Audio Routing**: Direct sound from Chrome to headphones, Spotify to speakers, or a game to a dedicated DAC.
- 🔌 **Smart Hardware Hotplug Detection**: Native Windows Core Audio callbacks (`IMMNotificationClient`) automatically re-enforce custom routing when devices are reconnected.
- 🎚️ **Per-App Volume & Mute**: Fine-tune volume sliders or toggle mute for individual running applications directly from the sleek tray flyout.
- 💾 **Persistent Routing Rules**: Persists rules to `%AppData%\AudioRouter\rules.json` and reapplies them whenever applications launch.
- ⏰ **Start with Windows**: Toggle autostart via user registry without requiring administrator privileges.

### 🛡️ Security & Privacy

- **100% Offline**: No network calls, zero telemetry, zero tracking.
- **Least Privilege**: Runs under standard user privileges (`PROCESS_QUERY_LIMITED_INFORMATION`), without requiring Administrator rights.
- **Local Storage**: Routing rules are stored strictly on your local disk.

### 📋 System Requirements

- **Operating System**: Windows 10 (1903+) or Windows 11 (64-bit).
- **Dependencies**: None.

### 🛠️ Building from Source

#### 1. Rust Native Build (Recommended)
```bash
git clone https://github.com/Yuutiro-sama/AudioRouterApp.git
cd AudioRouterApp/AudioRouterRust
cargo test
cargo build --release
```
The optimized executable will be generated at `AudioRouterRust/target/release/AudioRouter.exe`.

#### 2. C# .NET 10 Build
```bash
cd AudioRouterApp
dotnet build
```

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).
