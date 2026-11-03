# Audio Router App 🎧

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-lightgrey.svg)](https://microsoft.com/windows)
[![.NET](https://img.shields.io/badge/.NET-10.0-purple.svg)](https://dotnet.microsoft.com/)
[![Memory Footprint](https://img.shields.io/badge/RAM-%3C%205%20MB-brightgreen.svg)](#-ưu-điểm-nổi-bật)

> [!NOTE]  
> **Tuyên bố về AI và tính thương mại:**  
> Dự án này được phát triển với sự hỗ trợ của các mô hình AI phục vụ mục đích nghiên cứu, học tập và thử nghiệm ý tưởng (PoC). Mã nguồn và tài liệu do AI sinh ra chỉ đóng vai trò là khung tham khảo; dự án không được thiết kế sẵn cho mục đích thương mại trực tiếp và chưa được kiểm thử toàn diện cho môi trường vận hành thực tế (production).
>
> **AI and Commercial Disclaimer:**  
> This project was developed with the assistance of AI models for research, educational, and Proof-of-Concept (PoC) purposes. The AI-generated source code and documentation serve as a reference framework; this project is not designed for direct commercial deployment and has not been extensively tested for production environments.

---

[🇻🇳 Tiếng Việt](#-tiếng-việt) | [🇬🇧 English](#-english)

---

## 🇻🇳 Tiếng Việt

**Audio Router App** là một tiện ích mã nguồn mở nhỏ gọn dành cho Windows (xây dựng trên **WPF / .NET 10**), cho phép bạn định tuyến âm thanh của từng ứng dụng riêng biệt đến các thiết bị phát khác nhau (tai nghe, loa ngoài, USB DAC, thiết bị ảo), tùy chỉnh âm lượng từng app và tự động khôi phục cấu hình ngay khi cắm hoặc rút thiết bị.

### ⚡ Ưu điểm nổi bật

- 🚀 **Siêu nhẹ - Chỉ tốn < 5 MB RAM khi chạy nền**: Ứng dụng được tối ưu hóa sâu với bộ đệm Win32 (`ProcessInfoCache`, `IconCache`) và giải phóng tài nguyên triệt để, chiếm chưa tới **5 MB RAM** và gần như **0% CPU** khi ẩn dưới khay hệ thống (System Tray). Bạn hoàn toàn yên tâm treo máy chơi game hoặc làm việc nặng mà không lo tốn tài nguyên.
- 🎯 **Định tuyến âm thanh độc lập từng app**: Cho phép nghe nhạc từ trình duyệt qua tai nghe, phát game ra loa ngoài hoặc chuyển âm thanh Discord sang tai nghe đàm thoại mà không bị lẫn lộn.
- 🔌 **Tự động nhận diện khi cắm/rút thiết bị (Hardware Hotplug)**:
  - Tích hợp lắng nghe sự kiện từ Windows Core Audio (`IMMNotificationClient`).
  - Khi bạn rút tai nghe, Windows tự chuyển âm thanh tạm thời về loa; khi bạn cắm lại, app tự động khôi phục âm thanh về đúng thiết bị đó ngay lập tức (kèm bộ lọc chống xung đột tín hiệu).
- 🎚️ **Bộ trộn âm lượng (Per-App Volume & Mute)**: Điều chỉnh âm lượng và bật/tắt tiếng từng ứng dụng trực tiếp trên popup khay hệ thống, không cần vào Windows Settings rườm rà.
- 💾 **Tự động ghi nhớ cấu hình**: Lưu quy tắc vào `%AppData%\AudioRouter\rules.json` và tự áp dụng lại bất cứ khi nào ứng dụng đó mở lên.
- ⏰ **Khởi động cùng Windows tiện lợi**: Bật/tắt tự khởi động trực tiếp qua menu chuột phải khay hệ thống, hoàn toàn không đòi hỏi quyền Administrator.

### 🛡️ Bảo mật & Quyền riêng tư

- **100% Ngoại tuyến (Offline)**: Ứng dụng không gửi hay nhận bất kỳ gói tin mạng nào, không chứa mã theo dõi hay phân tích hành vi người dùng (No Telemetry).
- **Nguyên tắc đặc quyền tối thiểu (Least Privilege)**: Chạy hoàn toàn dưới quyền người dùng thông thường (`PROCESS_QUERY_LIMITED_INFORMATION`), không yêu cầu Admin hay can thiệp sâu vào nhân hệ điều hành.
- **An toàn dữ liệu**: Chỉ lưu tên tiến trình và mã thiết bị trên ổ cứng cá nhân của bạn. Xem chi tiết tại [SECURITY.md](SECURITY.md).

### 📋 Yêu cầu hệ thống

- **Hệ điều hành**: Windows 10 (bản 1809 trở lên) hoặc Windows 11 (64-bit).
- **Môi trường chạy**:
  - Với bản thông thường: Cần cài đặt [.NET 10 Desktop Runtime](https://dotnet.microsoft.com/download/dotnet/10.0).
  - Với bản đóng gói 1 file duy nhất (Single-File Self-Contained): Không cần cài bất kỳ phần mềm nào khác, mở lên chạy ngay.

### 🛠️ Hướng dẫn build từ mã nguồn

```bash
# 1. Clone repository
git clone https://github.com/yuutiro/AudioRouterApp.git
cd AudioRouterApp

# 2. Build bản Debug
dotnet build

# 3. Đóng gói ra 1 file exe duy nhất (không cần cài .NET)
dotnet publish AudioRouterApp/AudioRouterApp.csproj -c Release -r win-x64 --self-contained true -p:PublishSingleFile=true -p:IncludeNativeLibrariesForSelfExtract=true -p:EnableCompressionInSingleFile=true -o ./PublishSingleFile
```
File thực thi độc lập sẽ xuất hiện tại thư mục `./PublishSingleFile/AudioRouterApp.exe`.

### 🚀 Hướng dẫn sử dụng

1. Chạy file `AudioRouterApp.exe`.
2. Biểu tượng loa màu xanh sẽ xuất hiện dưới góc phải thanh Taskbar (khay hệ thống).
3. Nhấp chuột trái vào biểu tượng để mở bảng điều khiển.
4. Chọn thiết bị âm thanh mong muốn cho từng phần mềm đang phát âm thanh.

---

## 🇬🇧 English

**Audio Router App** is a lightweight, modern Windows desktop utility built with **WPF (.NET 10)** that allows you to route audio from specific applications to specific output devices (headphones, speakers, DACs, virtual cables) independently, adjust per-app volume, and automatically restore routing when devices are plugged or unplugged.

### ⚡ Key Advantages & Features

- 🚀 **Ultra-Lightweight (< 5 MB RAM Background Footprint)**: Deeply optimized using low-level Win32 process metadata caching (`ProcessInfoCache`), frozen bitmap caching (`IconCache`), and aggressive resource cleanup. Consumes **< 5 MB RAM** and near **0% CPU** while idling in the System Tray, ensuring zero impact on gaming or heavy workloads.
- 🎯 **Per-Application Audio Routing**: Direct sound from Chrome to headphones, Spotify to speakers, or a game to a dedicated DAC.
- 🔌 **Smart Hardware Hotplug Detection**:
  - Implements native Windows Core Audio callbacks (`IMMNotificationClient`).
  - Seamlessly and automatically restores per-app routing rules when a headset or USB DAC is reconnected, complete with event debouncing.
- 🎚️ **Per-App Volume & Mute**: Fine-tune volume sliders or toggle mute for individual running applications directly from a sleek tray popup.
- 💾 **Persistent Routing Rules**: Automatically persists your preferences in `%AppData%\AudioRouter\rules.json` and enforces them whenever target apps launch.
- ⏰ **Start with Windows**: Toggle autostart cleanly via user registry without requiring administrator privileges.

### 🛡️ Security & Privacy

- **100% Offline**: No network requests, no telemetry, no tracking, no analytics.
- **Least Privilege**: Runs entirely under standard user privileges (`PROCESS_QUERY_LIMITED_INFORMATION`), without requiring Administrator rights.
- **Safe Local Storage**: Only process names and endpoint device IDs are saved locally. See [SECURITY.md](SECURITY.md) for details.

### 📋 System Requirements

- **Operating System**: Windows 10 (version 1809+ / 21H2+) or Windows 11 (64-bit).
- **Runtime**:
  - For standard build: [.NET 10 Desktop Runtime](https://dotnet.microsoft.com/download/dotnet/10.0)
  - For standalone single-file build: No additional runtime required.

### 🛠️ How to Build from Source

```bash
# 1. Clone the repository
git clone https://github.com/yuutiro/AudioRouterApp.git
cd AudioRouterApp

# 2. Build Debug
dotnet build

# 3. Publish Single-File Executable (Standalone, Self-Contained)
dotnet publish AudioRouterApp/AudioRouterApp.csproj -c Release -r win-x64 --self-contained true -p:PublishSingleFile=true -p:IncludeNativeLibrariesForSelfExtract=true -p:EnableCompressionInSingleFile=true -o ./PublishSingleFile
```
The resulting standalone executable will be located in `./PublishSingleFile/AudioRouterApp.exe`.

### 🚀 How to Run

1. Launch `AudioRouterApp.exe`.
2. Look for the blue speaker icon in your Windows Taskbar System Tray (near the clock).
3. Click the icon to view all active audio applications.
4. Select the desired audio output device from the dropdown menu for any app.

### 🏗️ Architecture Overview

- **Core Audio Routing**: Communicates with internal Windows Audio Policy via undocumented COM factory `Windows.Media.Internal.AudioPolicyConfig` (`SetPersistedDefaultAudioEndpoint`).
- **Audio Sessions**: Managed via `NAudio.CoreAudioApi` (`MMDeviceEnumerator`, `AudioSessionManager`).
- **Device State Monitoring**: Custom COM implementation of `IMMNotificationClient` registered with Windows Core Audio MMDevAPI.

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).
