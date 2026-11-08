<p align="center">
  <img src="app_icon.png" width="128" height="128" alt="Audio Router App Icon" />
</p>

# Audio Router App 🎧 (v3.0.0)

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-lightgrey.svg)](https://microsoft.com/windows)
[![Rust](https://img.shields.io/badge/Rust-Native%20Win32%2FCOM%2FDSP-orange.svg)](https://www.rust-lang.org/)
[![Binary Size](https://img.shields.io/badge/Binary-~490%20KB-success.svg)](#-ưu-điểm-nổi-bật)
[![Memory Footprint](https://img.shields.io/badge/RAM-%3C%205%20MB-brightgreen.svg)](#-ưu-điểm-nổi-bật)

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

**Audio Router App** là tiện ích mã nguồn mở siêu nhẹ dành cho Windows, cho phép **phát âm thanh đồng thời ra nhiều thiết bị đầu ra cùng lúc** (loa laptop, tai nghe dây 3.5mm, tai nghe Bluetooth...), điều chỉnh **âm lượng độc lập cho từng thiết bị** và **từng ứng dụng**, đồng thời định tuyến luồng âm thanh linh hoạt và tự động khôi phục cấu hình ngay khi cắm hoặc rút thiết bị.

> [!TIP]
> **Phiên bản mới nhất (v3.0.0)** được phát triển hoàn toàn bằng **Rust thuần (Native Win32/COM/WASAPI)**, tích hợp **Bộ xử lý tín hiệu số thời gian thực (Real-time DSP Engine)** với thuật toán nội suy Cubic Hermite và bộ điều khiển PI bù trôi nhịp thạch anh (Clock Drift Compensation), không cần cài thêm driver ảo (Virtual Audio Cable) hay quyền Administrator!

### ⚡ Tính năng đột phá trên bản v3.0.0

- 🔊 **Phát âm thanh đa thiết bị đầu ra đồng thời (Simultaneous Multi-Output)**:
  - Cho phép tích chọn cùng lúc nhiều thiết bị phát: Loa máy tính, Tai nghe có dây 3.5mm, Tai nghe Bluetooth, USB DAC...
  - Không cần cài bất kỳ driver âm thanh ảo của bên thứ ba nào; hoạt động trực tiếp qua cơ chế **Zero-Copy WASAPI Loopback Fan-out**.
- 🎚️ **Thanh trượt âm lượng độc lập cho từng thiết bị**:
  - Mỗi thiết bị đầu ra có thanh trượt âm lượng (0 - 100%) và nút Mute riêng biệt.
  - Tự do tăng nhỏ loa ngoài mà tai nghe bluetooth vẫn giữ nguyên âm lượng tùy thích.
- 🎯 **Điều khiển âm lượng và định tuyến riêng cho từng App (Active Sessions)**:
  - Nhận diện tức thì tất cả ứng dụng đang phát âm thanh kèm biểu tượng icon và PID.
  - Lựa chọn phát đa đầu ra `"🔊 Phát Đa Đầu Ra (Mặc định)"` hoặc ghim ứng dụng đó ra một thiết bị chỉ định duy nhất (VD: Game ra tai nghe, Spotify ra loa).
  - Thanh trượt âm lượng và nút Mute độc lập cho từng ứng dụng.
- ⚙️ **Bộ xử lý DSP thích ứng (Adaptive Fractional Resampler & PI Clock Drift Controller)**:
  - Khắc phục triệt để hiện tượng lệch pha và trôi xung nhịp giữa các card âm thanh khác nhau bằng bộ điều khiển tỷ lệ sai số PI ($K_p = 0.00025, K_i = 0.0000015$).
  - Độ trễ siêu thấp (< 15ms), bộ làm mượt âm lượng một cực khử hoàn toàn tiếng nổ xẹt (Zero-Zipper Noise).
- 🚀 **Siêu nhẹ - File chạy ~455 KB, RAM < 2.5 MB**:
  - Khi ẩn ở khay hệ thống (System Tray), app chỉ tốn khoảng **1.5 - 2.5 MB RAM** và **0% CPU**.
- 🔌 **Tự động nhận diện khi cắm/rút thiết bị (Hardware Hotplug)**:
  - Lắng nghe trực tiếp sự kiện từ Windows Core Audio (`IMMNotificationClient`).
  - Tự động ghi nhớ và khôi phục âm thanh về đúng thiết bị ngay khi cắm lại.
- 💾 **Lưu cấu hình an toàn nguyên tử (Atomic Config)**:
  - Tự động lưu thiết lập đa thiết bị và âm lượng vào `%AppData%\AudioRouter\rules_v3.json` bằng kỹ thuật ghi tệp nguyên tử chống hỏng file khi tắt máy đột ngột.

### 🛡️ Bảo mật & Quyền riêng tư

- **100% Ngoại tuyến (Offline)**: Hoàn toàn không kết nối internet, không thu thập dữ liệu (No Telemetry).
- **Nguyên tắc đặc quyền tối thiểu (Least Privilege)**: Chạy hoàn toàn dưới quyền người dùng thông thường, không yêu cầu Run as Administrator.

### 📋 Yêu cầu hệ thống

- **Hệ điều hành**: Windows 10 (bản 1903 trở lên) hoặc Windows 11 (64-bit).
- **Phần mềm phụ trợ**: **Không cần cài đặt thêm bất cứ gì**.

### 🛠️ Hướng dẫn build từ mã nguồn

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

---

## 🇬🇧 English

**Audio Router App** is a high-performance, ultra-lightweight Windows utility that enables **simultaneous audio output to multiple devices** (laptop speakers, 3.5mm wired headphones, Bluetooth headphones, USB DACs), provides **independent hardware volume controls per device**, and offers **per-application volume and routing controls** without requiring external audio driver installations or administrative privileges.

> [!TIP]
> **Version 3.0.0** is crafted in pure native Rust with an embedded **Real-time DSP Mixing Engine** featuring 4-point Cubic Hermite fractional interpolation and a PI Clock Drift Controller, achieving sub-15ms sync without audio artifacts.

### ⚡ Key Features in v3.0.0

- 🔊 **Simultaneous Multi-Output Audio**: Play sound through speakers, wired headphones, and Bluetooth headsets at the exact same time.
- 🎚️ **Per-Device Volume & Mute**: Fine-tune independent volume sliders and mute buttons for each physical output endpoint.
- 🎯 **Per-App Volume & Routing**: Route individual apps to designated outputs or let them share the multi-output mix, with dedicated per-app volume sliders.
- ⚙️ **Hardware Clock Drift Compensation**: Closed-loop PI controller prevents buffer underrun/overrun across asynchronous crystal clocks.
- 🚀 **Ultra-Lightweight Footprint**: ~455 KB single standalone binary, idling at `< 2.5 MB` RAM.
- 🔌 **Dynamic Hardware Hotplug**: Instant reconnection detection and state restoration via WASAPI notification clients.

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).
