# Audio Router v3.0.0 — Simultaneous Multi-Output Routing, Real-Time DSP & Fluent UI

Bản phát hành chính thức **v3.0.0** mang đến bước nhảy vọt toàn diện về kiến trúc âm thanh và giao diện người dùng: hỗ trợ **định tuyến đa thiết bị đầu ra đồng thời (Simultaneous Multi-Output Routing)** cho từng ứng dụng, bộ xử lý tín hiệu số thời gian thực (**Real-Time DSP Resampler & PI Clock Drift Controller**), giao diện hiện đại **Windows 11 Fluent Design** và biểu tượng ứng dụng độc quyền.

---

## 🌟 Điểm nổi bật trong phiên bản v3.0.0 (Key Highlights)

### 🔊 1. Định Tuyến Âm Thanh Đa Thiết Bị Đầu Ra (Dual-Endpoint Routing per App)
- **Đầu ra kép độc lập**: Cho phép từng ứng dụng phát đồng thời ra 2 thiết bị âm thanh khác nhau (ví dụ: vừa nghe nhạc qua Tai nghe dây / Bluetooth, vừa phát âm thanh qua Loa ngoài laptop hoặc màn hình rời).
- **Công tắc Bật/Tắt linh hoạt**: Nút gạt chuyển đổi trạng thái `[Bật]` / `[Đã tắt]` nhanh cho đầu ra phụ mà không cần ngắt kết nối thiết bị.
- **Điều khiển âm lượng chuẩn xác**: Tinh chỉnh mức âm lượng riêng biệt cho từng thiết bị và từng ứng dụng từ 0% đến 100% với bước nhảy mượt mà 5%.

### ⚡ 2. Bộ Xử Lý Tín Hiệu Số Thời Gian Thực (High-Fidelity Real-Time DSP Engine)
- **Hermite Cubic Resampler**: Tự động chuyển đổi và nội suy tần số lấy mẫu giữa các thiết bị có tốc độ khác biệt (44.1 kHz, 48 kHz, 96 kHz, 192 kHz và các USB DAC Hi-Fi 384 kHz) với độ méo hài tổng (THD) cực thấp.
- **PI Clock Drift Controller**: Thuật toán kiểm soát trôi xung nhịp phần cứng chủ động, giữ độ trễ đồng pha (latency) dưới 15ms mà không gây hiện tượng nổ lách tách (crackle/pop) hay giật tiếng.
- **Zero-Underrun RingBuffer**: Cấu trúc bộ đệm vòng khóa không đồng bộ (Lock-free Ring Buffer) loại bỏ hoàn toàn hiện tượng tràn/hụt bộ đệm, giải tỏa triệt để tải CPU của tiến trình `audiodg.exe` (*Windows Audio Device Graph Isolation*).

### 🎨 3. Giao Diện Windows 11 Fluent Design & Tối Ưu Trải Nghiệm (UX/UI)
- **Giao diện Dark Mode cao cấp**: Tông màu xám đen hiện đại theo ngôn ngữ thiết kế Windows 11 Fluent UI, chữ to rõ ràng, bố cục trực quan.
- **Thanh cuộn mượt mà (Smooth Vertical Scrolling)**: Dễ dàng cuộn lăn chuột điều hướng khi có nhiều ứng dụng âm thanh đang chạy cùng lúc.
- **Khử trùng lặp phiên âm thanh (Session Deduplication)**: Tự động gom nhóm các phiên âm thanh của cùng một tiến trình (như Discord, Brave browser) theo PID/Session Identifier chuẩn xác, không còn hiện tượng trùng lặp.
- **Lọc thông minh**: Tự động ẩn tiến trình nội bộ của chính ứng dụng khỏi danh sách điều hướng.
- **Biểu tượng ứng dụng tùy chỉnh (Custom App Icon)**: Logo âm thanh sóng nhạc Fluent Squircle được nhúng trực tiếp vào tài nguyên file PE `.exe`, hiển thị sắc nét trên Taskbar, Alt-Tab và System Tray.

### 🛡️ 4. Siêu Nhẹ & An Toàn Tuyệt Đối (Zero Bloat, Standalone)
- **File đơn chạy ngay (.exe standalone)**: Không cần cài đặt phần mềm bên thứ ba hay driver âm thanh ảo (như VB-Cable/Virtual Audio Cable).
- **Dung lượng siêu nhỏ**: Chỉ **~490 KB** duy nhất một tệp thực thi.
- **Chiếm dụng tài nguyên cực thấp**: RAM **< 5 MB**, CPU **~0%**, khởi động tức thì.
- **An toàn & Bảo mật**: 100% mã nguồn mở Rust, hoạt động offline hoàn toàn, không yêu cầu quyền Administrator.

---

## 📦 Tải về (Download)

| Tệp tải về | Dung lượng | Mô tả |
| :--- | :--- | :--- |
| **`AudioRouter.exe`** | **~490 KB** | Bản thực thi Standalone duy nhất (chỉ cần tải về và nhấp đúp để chạy) |

---

## 🚀 Hướng dẫn sử dụng nhanh (Quick Start)
1. Tải tệp **`AudioRouter.exe`** ở mục **Assets** bên dưới.
2. Nhấp đúp vào `AudioRouter.exe` để khởi chạy. Ứng dụng sẽ xuất hiện ở khay hệ thống (System Tray).
3. Nhấp vào icon ở khay hệ thống để mở bảng điều khiển:
   - **Đầu ra 1**: Chọn thiết bị âm thanh chính cho từng ứng dụng.
   - **Đầu ra 2**: Chọn thiết bị âm thanh phụ phát đồng thời và bấm `[Bật]` để kích hoạt.
   - Kéo thanh trượt để thay đổi mức âm lượng mong muốn.
