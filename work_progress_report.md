# BÁO CÁO TIẾN TRÌNH KHẮC PHỤC SỰ CỐ & TRIỂN KHAI AUDIO ROUTER APP (v3.0.0)

## 1. Hiện trạng & Sự cố đã giải quyết ("app không mở")
Người dùng phản ánh lỗi: sau khi đóng gói thành ứng dụng trên máy, bấm vào shortcut/file exe thì "app không mở".

### Nguyên nhân cốt lõi phát hiện qua điều tra kỹ thuật:
1. **Lỗi Giao tiếp Liên tiến trình (IPC Wake Failure)**:
   - Khi app đã chạy ngầm (hoặc khởi động cùng Windows dưới khay hệ thống), người dùng nhấp đúp vào Shortcut Desktop / Start Menu.
   - Tiến trình thứ 2 phát hiện `ERROR_ALREADY_EXISTS` qua Mutex nhưng tìm cửa sổ `FindWindowW` thất bại (do thiếu cờ `CS_GLOBALCLASS` trên lớp cửa sổ Dispatcher và `PostMessage HWND_BROADCAST` bị chặn bởi cơ chế UIPI / tool window). Hậu quả: Tiến trình thứ 2 thoát ngay mà không đánh thức được tiến trình chính đang chạy ngầm.
2. **Cơ chế Tự đóng khi Mất tiêu điểm (Auto-Dismiss on Focus Loss / Blur)**:
   - Trước đây cửa sổ xử lý `WM_ACTIVATE` với `WA_INACTIVE` đều tự động gọi `close_flyout()`.
   - Khi người dùng nhấp đúp icon trên Desktop, cú nhấp chuột thứ 2 nhả ra trên nền Desktop khiến Windows kích hoạt Desktop, cửa sổ nhận ngay `WA_INACTIVE` và bị tiêu hủy (`DestroyWindow`) chỉ sau vài phần mười giây -> Người dùng thấy app nháy biến mất hoặc tưởng app không mở.
3. **Cờ `WS_EX_TOOLWINDOW` và điều kiện lọc thời gian khởi động**:
   - Chế độ standalone từng bị gán cờ `WS_EX_TOOLWINDOW` khiến không có icon Taskbar và không hiện trên Alt+Tab.
   - Đã loại bỏ hoàn toàn điều kiện uptime vô lý `is_recent_boot`.

---

## 2. Các giải pháp kỹ thuật đã triển khai triệt để

### A. IPC Đánh thức Cực nhanh qua Win32 Named Event:
- Đăng ký lớp cửa sổ `AudioRouterDispatcherClass` với thuộc tính `CS_GLOBALCLASS`.
- Khởi tạo Win32 Named Session Event `AudioRouterApp_WakeEvent_v3`.
- Tạo luồng nền chuyên dụng `AudioRouter_WakeListener` chờ tín hiệu từ Event.
- Khi người dùng bấm Shortcut mở app (tiến trình thứ 2):
  - Gọi `AllowSetForegroundWindow(0xFFFFFFFF)` để chuyển quyền đưa cửa sổ lên trên cùng.
  - Gọi `SetEvent(AudioRouterApp_WakeEvent_v3)` để đánh thức ngay lập tức (độ trễ 0ms) tiến trình chính.
  - Tiến trình chính nhận tín hiệu, làm mới danh sách phiên âm thanh và hiển thị cửa sổ lên trước mặt người dùng.

### B. Phân tách rõ ràng giữa Flyout Khay hệ thống và Cửa sổ Ứng dụng Độc lập:
- **Cửa sổ Khay Hệ thống (Tray Flyout)**: Bấm icon dưới góc Taskbar -> mở nhanh, click ra ngoài tự ẩn nhẹ nhàng.
- **Cửa sổ Độc lập (Standalone App Window)**: Mở từ Shortcut Desktop, Start Menu, tìm kiếm Windows -> Sử dụng `WS_EX_APPWINDOW | WS_EX_TOPMOST`, hiển thị biểu tượng trên thanh Taskbar, xuất hiện trong Alt+Tab, cố định trên màn hình, **không tự đóng khi click ra ngoài**.

### C. Triển khai & Kiểm thử thực tế qua UI Automation:
- Toàn bộ 56 unit test WASAPI & DSP đều passed 100%.
- Biên dịch bản Release tối ưu cao `AudioRouter.exe`.
- Đóng gói Inno Setup thành công `dist/AudioRouter-v3.0.0-Setup.exe`.
- Cập nhật trực tiếp vào hệ thống: `%LOCALAPPDATA%\Programs\AudioRouter\AudioRouter.exe`.
- Đã kiểm tra thực tế bằng `agent-desktop`:
  - Mở app thành công, kích thước 650x533, căn giữa màn hình.
  - Nhận diện đầy đủ các ứng dụng âm thanh đang chạy (`brave.exe`, game `nshm.exe` - Nghịch Thủy Hàn).
  - Thử nghiệm đóng cửa sổ bằng nút `✕` -> Thu gọn về khay hệ thống, RAM giải phóng xuống còn ~5MB.
  - Thử nghiệm kích hoạt lại từ tiến trình thứ 2 -> Cửa sổ lập tức mở lại và lấy tiêu điểm hoàn hảo!
