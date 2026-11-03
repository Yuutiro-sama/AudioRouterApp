using System;
using Microsoft.Win32;

namespace AudioRouterApp;

// Bật/tắt khởi động cùng Windows bằng Registry của riêng người dùng (không cần quyền admin)
static class StartupManager
{
    private const string RunKey = @"Software\Microsoft\Windows\CurrentVersion\Run";
    private const string ValueName = "AudioRouter";

    public static bool IsEnabled()
    {
        using var key = Registry.CurrentUser.OpenSubKey(RunKey);
        return key?.GetValue(ValueName) != null;
    }

    public static void SetEnabled(bool enable)
    {
        using var key = Registry.CurrentUser.OpenSubKey(RunKey, writable: true);
        if (key == null) return;

        if (enable)
        {
            string? exe = Environment.ProcessPath;
            if (exe != null) key.SetValue(ValueName, "\"" + exe + "\"");
        }
        else
        {
            key.DeleteValue(ValueName, throwOnMissingValue: false);
        }
    }
}