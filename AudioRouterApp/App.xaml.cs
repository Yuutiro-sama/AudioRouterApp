using System;
using System.Threading;
using System.Windows;

namespace AudioRouterApp;

public partial class App : Application
{
    private static Mutex? _mutex;
    private System.Windows.Forms.NotifyIcon? _tray;
    private MainWindow? _popup;

    protected override void OnStartup(StartupEventArgs e)
    {
        base.OnStartup(e);
        ShutdownMode = ShutdownMode.OnExplicitShutdown; // không tắt app khi popup bị ẩn

        // Chỉ cho phép một bản chạy cùng lúc
        _mutex = new Mutex(true, "AudioRouterApp_SingleInstance", out bool isNewInstance);
        if (!isNewInstance)
        {
            Shutdown();
            return;
        }

        _popup = new MainWindow();

        _tray = new System.Windows.Forms.NotifyIcon
        {
            Icon = TrayIconFactory.Create(),
            Text = "Audio Router",
            Visible = true
        };

        _tray.MouseClick += (s, args) =>
        {
            if (args.Button == System.Windows.Forms.MouseButtons.Left)
                _popup.ToggleNearTray();
        };

        var menu = new System.Windows.Forms.ContextMenuStrip();
        menu.Items.Add("Áp dụng tất cả quy tắc", null, (s, a) => _popup.ApplyAllRules(true));

        var startupItem = new System.Windows.Forms.ToolStripMenuItem("Khởi động cùng Windows")
        {
            CheckOnClick = true,
            Checked = StartupManager.IsEnabled()
        };
        startupItem.CheckedChanged += (s, a) => StartupManager.SetEnabled(startupItem.Checked);
        menu.Items.Add(startupItem);

        menu.Items.Add(new System.Windows.Forms.ToolStripSeparator());
        menu.Items.Add("Thoát", null, (s, a) => ExitApp());
        _tray.ContextMenuStrip = menu;

        _popup.ApplyAllRules(); // áp dụng quy tắc đã lưu ngay khi khởi động
        AudioRouterApp.MainWindow.TrimMemory();
    }

    private void ExitApp()
    {
        if (_tray != null)
        {
            _tray.Visible = false;
            _tray.Dispose();
            _tray = null;
        }
        _popup?.Dispose();
        Shutdown();
    }

    protected override void OnExit(ExitEventArgs e)
    {
        _tray?.Dispose();
        _popup?.Dispose();
        base.OnExit(e);
    }
}