using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Linq;
using System.Runtime.InteropServices;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Input;
using System.Windows.Media;
using System.Windows.Threading;
using NAudio.CoreAudioApi;

namespace AudioRouterApp;

public partial class MainWindow : Window, IDisposable
{
    private readonly MMDeviceEnumerator _enumerator = new MMDeviceEnumerator();
    private readonly AudioDeviceMonitor _deviceMonitor = new AudioDeviceMonitor();
    private readonly Dictionary<string, string> _rules = RuleStore.Load();
    private readonly HashSet<string> _applied = new HashSet<string>(); // các cặp "pid|thiết bị" đã gán

    // Giữ tham chiếu các đối tượng âm thanh khi popup đang mở để phục vụ thanh trượt volume/mute
    private List<MMDevice> _currentDevices = new();
    private List<AppEntry> _currentApps = new();

    // Cache danh sách thiết bị active để không phải EnumAudioEndpoints mỗi 3 giây
    private HashSet<string> _activeDeviceIds = new(StringComparer.OrdinalIgnoreCase);
    private int _idleTicks = 0;

    private readonly DispatcherTimer _pollTimer;
    private readonly DispatcherTimer _deviceChangeDebounceTimer;
    private bool _loading;
    private int _openDropdowns;
    private bool _pendingRefresh;
    private DateTime _lastHide = DateTime.MinValue;

    #region Win32 API
    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint processAccess, bool bInheritHandle, uint processId);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool GetExitCodeProcess(IntPtr hProcess, out uint lpExitCode);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(IntPtr hObject);

    [DllImport("psapi.dll")]
    private static extern int EmptyWorkingSet(IntPtr hwProc);

    private const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;
    private const uint STILL_ACTIVE = 259;
    #endregion

    public MainWindow()
    {
        InitializeComponent();

        // Cập nhật danh sách thiết bị ban đầu
        UpdateActiveDeviceIds();

        // Lắng nghe sự kiện cắm/rút/thay đổi thiết bị âm thanh từ Windows
        _deviceMonitor.DeviceChanged += OnAudioDeviceChanged;
        _deviceMonitor.Start();

        // Timer debounce 600ms để đợi Windows và driver âm thanh sẵn sàng sau khi cắm/rút
        _deviceChangeDebounceTimer = new DispatcherTimer { Interval = TimeSpan.FromMilliseconds(600) };
        _deviceChangeDebounceTimer.Tick += OnDeviceChangeDebounced;

        // Quét định kỳ cực nhẹ (chỉ kiểm tra process, không gọi COM/AudioSessionManager nên 0 leak RAM)
        _pollTimer = new DispatcherTimer { Interval = TimeSpan.FromSeconds(3) };
        _pollTimer.Tick += (s, e) =>
        {
            ApplyAllRules();
            if (!IsVisible && ++_idleTicks % 20 == 0) // Cứ 1 phút thu hồi bộ nhớ dư khi chạy ngầm
            {
                TrimMemory();
            }
        };
        _pollTimer.Start();
    }

    public static void TrimMemory()
    {
        try
        {
            GC.Collect(2, GCCollectionMode.Forced, true);
            GC.WaitForPendingFinalizers();
            GC.Collect(2, GCCollectionMode.Forced, true);
            EmptyWorkingSet(Process.GetCurrentProcess().Handle);
        }
        catch { }
    }

    private void CleanupSessionObjects()
    {
        foreach (var app in _currentApps)
        {
            foreach (var s in app.Sessions)
            {
                try { s.Dispose(); } catch { }
            }
            app.Sessions.Clear();
        }
        _currentApps.Clear();

        foreach (var dev in _currentDevices)
        {
            try { dev.AudioSessionManager?.Dispose(); } catch { }
            try { dev.Dispose(); } catch { }
        }
        _currentDevices.Clear();
    }

    private void UpdateActiveDeviceIds()
    {
        var ids = new HashSet<string>(StringComparer.OrdinalIgnoreCase);
        try
        {
            using var devices = _enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active);
            foreach (var dev in devices)
            {
                try
                {
                    ids.Add(dev.ID);
                }
                finally
                {
                    dev.Dispose();
                }
            }
        }
        catch { }
        _activeDeviceIds = ids;
    }

    private static List<uint> GetPidsByAppName(string appName)
    {
        var result = new List<uint>();
        try
        {
            var procs = Process.GetProcessesByName(appName);
            foreach (var p in procs)
            {
                result.Add((uint)p.Id);
                p.Dispose();
            }
        }
        catch { }
        return result;
    }

    private static bool IsProcessAlive(uint pid)
    {
        IntPtr hProc = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (hProc == IntPtr.Zero) return false;
        try
        {
            if (GetExitCodeProcess(hProc, out uint exitCode))
            {
                return exitCode == STILL_ACTIVE;
            }
            return false;
        }
        finally
        {
            CloseHandle(hProc);
        }
    }

    private void OnAudioDeviceChanged(AudioDeviceEvent evt, string? deviceId)
    {
        Dispatcher.BeginInvoke(() =>
        {
            // Khi thiết bị cắm/rút hoặc đổi trạng thái/mặc định,
            // xóa cache các quy tắc liên quan để ép gán lại đúng khi phần cứng sẵn sàng.
            if (!string.IsNullOrEmpty(deviceId))
            {
                _applied.RemoveWhere(k => k.EndsWith("|" + deviceId, StringComparison.OrdinalIgnoreCase));
            }
            else
            {
                _applied.Clear();
            }

            // Reset debounce timer
            _deviceChangeDebounceTimer.Stop();
            _deviceChangeDebounceTimer.Start();
        });
    }

    private void OnDeviceChangeDebounced(object? sender, EventArgs e)
    {
        _deviceChangeDebounceTimer.Stop();

        // 1. Cập nhật lại danh sách thiết bị active sau khi cắm/rút
        UpdateActiveDeviceIds();

        // 2. Áp dụng lại quy tắc với cờ force để gán lại luồng âm thanh
        ApplyAllRules(force: true);

        // 3. Nếu popup đang mở: làm mới giao diện nếu người dùng không đang mở danh sách thả xuống
        if (IsVisible)
        {
            if (_openDropdowns == 0)
            {
                Refresh();
            }
            else
            {
                _pendingRefresh = true;
            }
        }
    }

    // Click icon: ẩn nếu đang hiện, hiện nếu đang ẩn
    public void ToggleNearTray()
    {
        if (IsVisible)
        {
            HideWindow();
            return;
        }

        // Bấm vào icon làm popup mất focus rồi tự ẩn; chặn việc nó bật lại ngay lập tức
        if ((DateTime.UtcNow - _lastHide).TotalMilliseconds < 300) return;

        Refresh();
        Show();
        UpdateLayout();

        var area = SystemParameters.WorkArea; // vùng làm việc (trừ taskbar), đơn vị đã quy đổi theo DPI
        Left = area.Right - ActualWidth;   // viền trong suốt 12px của cửa sổ tạo khoảng cách với mép
        Top = area.Bottom - ActualHeight;
        Activate();
    }

    public void HideWindow()
    {
        _lastHide = DateTime.UtcNow;
        Hide();
        AppList.Children.Clear();
        CleanupSessionObjects();
        TrimMemory();
    }

    // Dựng lại danh sách app đang có phiên âm thanh
    private void Refresh()
    {
        _loading = true;
        try
        {
            AppList.Children.Clear();
            CleanupSessionObjects();

            using var devCol = _enumerator.EnumerateAudioEndPoints(DataFlow.Render, DeviceState.Active);
            _currentDevices = devCol.ToList();
            var items = _currentDevices.Select(d => new DeviceItem(d.ID, d.FriendlyName)).ToList();
            _currentApps = AudioService.GetApps(_currentDevices);

            EmptyText.Visibility = _currentApps.Count == 0 ? Visibility.Visible : Visibility.Collapsed;

            foreach (var app in _currentApps)
                AppList.Children.Add(BuildRow(app, items));

            StatusText.Text = "";
        }
        catch (Exception ex)
        {
            StatusText.Text = "Lỗi: " + ex.Message;
        }
        finally
        {
            _loading = false;
        }
    }

    // Một thẻ cho mỗi app: icon, tên, ô chọn thiết bị, nút tắt tiếng, thanh âm lượng
    private UIElement BuildRow(AppEntry app, List<DeviceItem> items)
    {
        var card = new Border
        {
            Background = Brushes.White,
            CornerRadius = new CornerRadius(8),
            Padding = new Thickness(10),
            Margin = new Thickness(0, 0, 0, 8)
        };

        var grid = new Grid();
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(38) });
        grid.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });

        var img = new Image
        {
            Width = 28,
            Height = 28,
            VerticalAlignment = VerticalAlignment.Top,
            Source = LoadIcon(app.ExePath)
        };
        Grid.SetColumn(img, 0);
        grid.Children.Add(img);

        var panel = new StackPanel();
        Grid.SetColumn(panel, 1);

        panel.Children.Add(new TextBlock
        {
            Text = app.Name,
            FontWeight = FontWeights.SemiBold,
            Foreground = new SolidColorBrush(Color.FromRgb(0x20, 0x20, 0x20)),
            Margin = new Thickness(0, 0, 0, 4)
        });

        // Ô chọn thiết bị
        var combo = new ComboBox
        {
            ItemsSource = items,
            DisplayMemberPath = "Name",
            Tag = app
        };

        // Nếu app có quy tắc với một thiết bị đang hoạt động, ưu tiên hiển thị thiết bị đó
        string selectedId = app.CurrentDeviceId;
        if (_rules.TryGetValue(app.Name, out var ruleDevId) && items.Any(i => i.Id == ruleDevId))
        {
            selectedId = ruleDevId;
        }
        combo.SelectedItem = items.FirstOrDefault(i => i.Id == selectedId) ?? items.FirstOrDefault();
        combo.SelectionChanged += Combo_SelectionChanged;
        combo.DropDownOpened += (s, e) => _openDropdowns++;
        combo.DropDownClosed += (s, e) =>
        {
            _openDropdowns--;
            Activate();
            if (_pendingRefresh && _openDropdowns == 0)
            {
                _pendingRefresh = false;
                Refresh();
            }
        };
        panel.Children.Add(combo);

        // Hàng âm lượng: [nút tắt tiếng] [thanh trượt] [số %]
        var volRow = new Grid { Margin = new Thickness(0, 6, 0, 0) };
        volRow.ColumnDefinitions.Add(new ColumnDefinition { Width = GridLength.Auto });
        volRow.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(1, GridUnitType.Star) });
        volRow.ColumnDefinitions.Add(new ColumnDefinition { Width = new GridLength(38) });

        bool muted = app.GetMute();
        var muteBtn = new Button
        {
            Content = muted ? "🔇" : "🔊",
            Width = 30,
            Height = 26,
            Background = Brushes.Transparent,
            BorderThickness = new Thickness(0),
            Cursor = Cursors.Hand,
            ToolTip = "Bật/tắt tiếng"
        };
        Grid.SetColumn(muteBtn, 0);

        double startValue = Math.Round(app.GetVolume() * 100);
        var slider = new Slider
        {
            Minimum = 0,
            Maximum = 100,
            Value = startValue,
            IsMoveToPointEnabled = true,
            VerticalAlignment = VerticalAlignment.Center,
            Margin = new Thickness(6, 0, 6, 0)
        };
        Grid.SetColumn(slider, 1);

        var pct = new TextBlock
        {
            Text = startValue + "%",
            TextAlignment = TextAlignment.Right,
            VerticalAlignment = VerticalAlignment.Center,
            Foreground = new SolidColorBrush(Color.FromRgb(0x55, 0x55, 0x55))
        };
        Grid.SetColumn(pct, 2);

        // Gắn sự kiện SAU khi đã đặt giá trị ban đầu, để không kích hoạt lúc dựng giao diện
        slider.ValueChanged += (s, e) =>
        {
            app.SetVolume((float)(e.NewValue / 100.0));
            pct.Text = Math.Round(e.NewValue) + "%";
        };
        muteBtn.Click += (s, e) =>
        {
            muted = !muted;
            app.SetMute(muted);
            muteBtn.Content = muted ? "🔇" : "🔊";
        };

        volRow.Children.Add(muteBtn);
        volRow.Children.Add(slider);
        volRow.Children.Add(pct);
        panel.Children.Add(volRow);

        grid.Children.Add(panel);
        card.Child = grid;
        return card;
    }

    // Lấy icon của file .exe với bộ nhớ đệm (cache), tránh đọc lại đĩa cứng nhiều lần
    private static ImageSource? LoadIcon(string? path) => IconCache.GetIcon(path);

    static class IconCache
    {
        private static readonly Dictionary<string, ImageSource?> _cache = new(StringComparer.OrdinalIgnoreCase);

        public static ImageSource? GetIcon(string? path)
        {
            if (string.IsNullOrEmpty(path)) return null;

            lock (_cache)
            {
                if (_cache.TryGetValue(path, out var cached))
                    return cached;
            }

            ImageSource? result = null;
            try
            {
                if (System.IO.File.Exists(path))
                {
                    using var icon = System.Drawing.Icon.ExtractAssociatedIcon(path);
                    if (icon != null)
                    {
                        var source = System.Windows.Interop.Imaging.CreateBitmapSourceFromHIcon(
                            icon.Handle,
                            Int32Rect.Empty,
                            System.Windows.Media.Imaging.BitmapSizeOptions.FromEmptyOptions());
                        source.Freeze(); // Cực kỳ quan trọng: GPU caching và thread-safe
                        result = source;
                    }
                }
            }
            catch { }

            lock (_cache)
            {
                _cache[path] = result;
            }

            return result;
        }
    }

    // Người dùng chọn thiết bị mới cho một app: gán ngay và lưu quy tắc
    private void Combo_SelectionChanged(object sender, SelectionChangedEventArgs e)
    {
        if (_loading) return;
        if (sender is not ComboBox combo || combo.Tag is not AppEntry app ||
            combo.SelectedItem is not DeviceItem dev) return;

        try
        {
            _rules[app.Name] = dev.Id;
            RuleStore.Save(_rules);

            int worst = 0;
            foreach (var pid in app.Pids)
            {
                int hr = AudioRouter.SetOutputDevice(pid, dev.Id);
                if (hr != 0) worst = hr;
                else _applied.Add(pid + "|" + dev.Id);
            }
            StatusText.Text = worst == 0 ? $"{app.Name} → {dev.Name}" : $"Lỗi 0x{worst:X8}";
        }
        catch (Exception ex)
        {
            StatusText.Text = "Lỗi: " + ex.Message;
        }
    }

    // Áp dụng tất cả quy tắc đã cấu hình cho các tiến trình tương ứng
    // Được tối ưu hoá hoàn toàn: chỉ kiểm tra các app có trong _rules, không gọi COM/AudioSessionManager nên 0 leak RAM.
    public void ApplyAllRules(bool force = false)
    {
        if (_rules.Count == 0 || _activeDeviceIds.Count == 0) return;

        try
        {
            // Dọn dẹp cache các tiến trình đã tắt khỏi _applied
            _applied.RemoveWhere(k =>
            {
                var parts = k.Split('|');
                if (uint.TryParse(parts[0], out var pid))
                {
                    return !IsProcessAlive(pid);
                }
                return true;
            });

            foreach (var rule in _rules)
            {
                if (!_activeDeviceIds.Contains(rule.Value)) continue; // thiết bị chưa cắm hoặc không active

                var pids = GetPidsByAppName(rule.Key);
                foreach (var pid in pids)
                {
                    string key = pid + "|" + rule.Value;
                    if (!force && _applied.Contains(key)) continue;

                    if (AudioRouter.SetOutputDevice(pid, rule.Value) == 0)
                    {
                        _applied.Add(key);
                    }
                }
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine("ApplyAllRules lỗi: " + ex.Message);
        }
    }

    public void Dispose()
    {
        _deviceChangeDebounceTimer.Stop();
        _pollTimer.Stop();
        _deviceMonitor.Dispose();
        CleanupSessionObjects();
        _enumerator.Dispose();
        GC.SuppressFinalize(this);
    }

    private void Window_Deactivated(object? sender, EventArgs e)
    {
        if (_openDropdowns > 0) return; // đang mở danh sách thả xuống thì đừng ẩn
        HideWindow();
    }

    private void Window_KeyDown(object sender, KeyEventArgs e)
    {
        if (e.Key == Key.Escape)
        {
            HideWindow();
        }
    }
}