using System;
using System.Collections.Concurrent;
using System.Collections.Generic;
using System.Diagnostics;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using NAudio.CoreAudioApi;
using NAudio.CoreAudioApi.Interfaces;


namespace AudioRouterApp;

[StructLayout(LayoutKind.Sequential)]
public struct PropertyKey
{
    public Guid fmtid;
    public int pid;
}

public enum AudioDeviceEvent
{
    Added,
    Removed,
    StateChanged,
    DefaultChanged
}

[ComImport]
[Guid("7991EEC9-7E89-4D85-8390-6C703CEC60C0")]
[InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IMMNotificationClient
{
    [PreserveSig] int OnDeviceStateChanged([MarshalAs(UnmanagedType.LPWStr)] string pwstrDeviceId, int dwNewState);
    [PreserveSig] int OnDeviceAdded([MarshalAs(UnmanagedType.LPWStr)] string pwstrDeviceId);
    [PreserveSig] int OnDeviceRemoved([MarshalAs(UnmanagedType.LPWStr)] string pwstrDeviceId);
    [PreserveSig] int OnDefaultDeviceChanged(int flow, int role, [MarshalAs(UnmanagedType.LPWStr)] string? pwstrDefaultDeviceId);
    [PreserveSig] int OnPropertyValueChanged([MarshalAs(UnmanagedType.LPWStr)] string pwstrDeviceId, PropertyKey key);
}

[ComImport]
[Guid("A95664D2-9614-4F35-A746-DE8DB63617E6")]
[InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IMMDeviceEnumerator
{
    [PreserveSig] int EnumAudioEndpoints(int dataFlow, int stateMask, out IntPtr devices);
    [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IntPtr endpoint);
    [PreserveSig] int GetDevice([MarshalAs(UnmanagedType.LPWStr)] string pwstrId, out IntPtr device);
    [PreserveSig] int RegisterEndpointNotificationCallback(IMMNotificationClient client);
    [PreserveSig] int UnregisterEndpointNotificationCallback(IMMNotificationClient client);
}

[ComImport]
[Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")]
public class MMDeviceEnumeratorComObject
{
}

public class AudioDeviceMonitor : IMMNotificationClient, IDisposable
{
    private IMMDeviceEnumerator? _enumerator;
    private bool _registered;

    public event Action<AudioDeviceEvent, string?>? DeviceChanged;

    public void Start()
    {
        if (_registered) return;
        try
        {
            _enumerator = (IMMDeviceEnumerator)new MMDeviceEnumeratorComObject();
            int hr = _enumerator.RegisterEndpointNotificationCallback(this);
            if (hr == 0)
            {
                _registered = true;
                Debug.WriteLine("Đã đăng ký lắng nghe thay đổi thiết bị âm thanh.");
            }
            else
            {
                Debug.WriteLine($"RegisterEndpointNotificationCallback lỗi: 0x{hr:X8}");
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine("Không khởi động được AudioDeviceMonitor: " + ex.Message);
        }
    }

    public void Stop()
    {
        if (!_registered || _enumerator == null) return;
        try
        {
            _enumerator.UnregisterEndpointNotificationCallback(this);
            _registered = false;
        }
        catch (Exception ex)
        {
            Debug.WriteLine("UnregisterEndpointNotificationCallback lỗi: " + ex.Message);
        }
        finally
        {
            _enumerator = null;
        }
    }

    public void Dispose()
    {
        Stop();
        GC.SuppressFinalize(this);
    }

    int IMMNotificationClient.OnDeviceStateChanged(string pwstrDeviceId, int dwNewState)
    {
        try { DeviceChanged?.Invoke(AudioDeviceEvent.StateChanged, pwstrDeviceId); } catch { }
        return 0;
    }

    int IMMNotificationClient.OnDeviceAdded(string pwstrDeviceId)
    {
        try { DeviceChanged?.Invoke(AudioDeviceEvent.Added, pwstrDeviceId); } catch { }
        return 0;
    }

    int IMMNotificationClient.OnDeviceRemoved(string pwstrDeviceId)
    {
        try { DeviceChanged?.Invoke(AudioDeviceEvent.Removed, pwstrDeviceId); } catch { }
        return 0;
    }

    int IMMNotificationClient.OnDefaultDeviceChanged(int flow, int role, string? pwstrDefaultDeviceId)
    {
        // Chỉ quan tâm render (flow == 0)
        if (flow == 0)
        {
            try { DeviceChanged?.Invoke(AudioDeviceEvent.DefaultChanged, pwstrDefaultDeviceId); } catch { }
        }
        return 0;
    }

    int IMMNotificationClient.OnPropertyValueChanged(string pwstrDeviceId, PropertyKey key)
    {
        return 0;
    }
}





public enum EDataFlow { eRender = 0, eCapture = 1, eAll = 2 }
public enum ERole { eConsole = 0, eMultimedia = 1, eCommunications = 2 }

// Giữ NGUYÊN thứ tự các hàm, không xóa, không đổi chỗ.
[ComImport]
[Guid("ab3d4648-e242-459f-b02f-541c70306324")]
[InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
public interface IAudioPolicyConfigFactoryVariantFor21H2
{
    [PreserveSig] int GetIids(out uint iidCount, out IntPtr iids);
    [PreserveSig] int GetRuntimeClassName(out IntPtr className);
    [PreserveSig] int GetTrustLevel(out int trustLevel);

    [PreserveSig] int __incomplete__add_CtxVolumeChange();
    [PreserveSig] int __incomplete__remove_CtxVolumeChanged();
    [PreserveSig] int __incomplete__add_RingerVibrateStateChanged();
    [PreserveSig] int __incomplete__remove_RingerVibrateStateChange();
    [PreserveSig] int __incomplete__SetVolumeGroupGainForId();
    [PreserveSig] int __incomplete__GetVolumeGroupGainForId();
    [PreserveSig] int __incomplete__GetActiveVolumeGroupForEndpointId();
    [PreserveSig] int __incomplete__GetVolumeGroupsForEndpoint();
    [PreserveSig] int __incomplete__GetCurrentVolumeContext();
    [PreserveSig] int __incomplete__SetVolumeGroupMuteForId();
    [PreserveSig] int __incomplete__GetVolumeGroupMuteForId();
    [PreserveSig] int __incomplete__SetRingerVibrateState();
    [PreserveSig] int __incomplete__GetRingerVibrateState();
    [PreserveSig] int __incomplete__SetPreferredChatApplication();
    [PreserveSig] int __incomplete__ResetPreferredChatApplication();
    [PreserveSig] int __incomplete__GetPreferredChatApplication();
    [PreserveSig] int __incomplete__GetCurrentChatApplications();
    [PreserveSig] int __incomplete__add_ChatContextChanged();
    [PreserveSig] int __incomplete__remove_ChatContextChanged();

    [PreserveSig]
    int SetPersistedDefaultAudioEndpoint(uint processId, EDataFlow flow, ERole role, IntPtr deviceId);
}

static class Combase
{
    [DllImport("combase.dll", PreserveSig = false)]
    public static extern void RoGetActivationFactory(
        IntPtr activatableClassId, [In] ref Guid iid, out IntPtr factory);

    [DllImport("combase.dll", PreserveSig = false)]
    public static extern void WindowsCreateString(
        [MarshalAs(UnmanagedType.LPWStr)] string src, uint length, out IntPtr hstring);

    [DllImport("combase.dll", PreserveSig = false)]
    public static extern void WindowsDeleteString(IntPtr hstring);
}

static class AudioRouter
{
    private const string DEVINTERFACE_AUDIO_RENDER = "#{e6327cad-dcec-4949-ae8a-991e976a79d2}";
    private const string MMDEVAPI_TOKEN = @"\\?\SWD#MMDEVAPI#";

    private static IAudioPolicyConfigFactoryVariantFor21H2? _factory;

    private static IAudioPolicyConfigFactoryVariantFor21H2 GetFactory()
    {
        if (_factory != null) return _factory;

        var iid = typeof(IAudioPolicyConfigFactoryVariantFor21H2).GUID;
        IntPtr className = IntPtr.Zero;
        IntPtr factoryPtr = IntPtr.Zero;
        try
        {
            const string cls = "Windows.Media.Internal.AudioPolicyConfig";
            Combase.WindowsCreateString(cls, (uint)cls.Length, out className);
            Combase.RoGetActivationFactory(className, ref iid, out factoryPtr);
            object obj = Marshal.GetObjectForIUnknown(factoryPtr);
            _factory = (IAudioPolicyConfigFactoryVariantFor21H2)obj;
        }
        finally
        {
            if (factoryPtr != IntPtr.Zero) Marshal.Release(factoryPtr);
            if (className != IntPtr.Zero) Combase.WindowsDeleteString(className);
        }
        return _factory!;
    }

    // Trả về 0 nếu thành công, ngược lại là mã lỗi HRESULT.
    public static int SetOutputDevice(uint processId, string deviceId)
    {
        var factory = GetFactory();
        IntPtr hstring = IntPtr.Zero;
        try
        {
            string full = MMDEVAPI_TOKEN + deviceId + DEVINTERFACE_AUDIO_RENDER;
            Combase.WindowsCreateString(full, (uint)full.Length, out hstring);

            int r1 = factory.SetPersistedDefaultAudioEndpoint(processId, EDataFlow.eRender, ERole.eMultimedia, hstring);
            int r2 = factory.SetPersistedDefaultAudioEndpoint(processId, EDataFlow.eRender, ERole.eConsole, hstring);
            return r1 != 0 ? r1 : r2;
        }
        finally
        {
            if (hstring != IntPtr.Zero) Combase.WindowsDeleteString(hstring);
        }
    }
}

// Lưu quy tắc dạng: tên app -> ID thiết bị. File: %AppData%\AudioRouter\rules.json
// (dùng chung file với bản console nên quy tắc cũ của bạn vẫn còn)
static class RuleStore
{
    public static readonly string FileLocation = Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData),
        "AudioRouter", "rules.json");

    public static Dictionary<string, string> Load()
    {
        try
        {
            if (File.Exists(FileLocation))
            {
                var json = File.ReadAllText(FileLocation);
                var data = JsonSerializer.Deserialize<Dictionary<string, string>>(json);
                if (data != null)
                    return new Dictionary<string, string>(data, StringComparer.OrdinalIgnoreCase);
            }
        }
        catch (Exception ex)
        {
            Debug.WriteLine("Không đọc được cấu hình: " + ex.Message);
        }
        return new Dictionary<string, string>(StringComparer.OrdinalIgnoreCase);
    }

    public static void Save(Dictionary<string, string> rules)
    {
        Directory.CreateDirectory(Path.GetDirectoryName(FileLocation)!);
        var json = JsonSerializer.Serialize(rules, new JsonSerializerOptions { WriteIndented = true });
        File.WriteAllText(FileLocation, json);
    }
}

public record DeviceItem(string Id, string Name);

public class AppEntry
{
    public string Name { get; }
    public string CurrentDeviceId { get; set; }
    public string? ExePath { get; set; }
    public HashSet<uint> Pids { get; } = new HashSet<uint>();
    public List<AudioSessionControl> Sessions { get; } = new List<AudioSessionControl>();

    public AppEntry(string name, string currentDeviceId)
    {
        Name = name;
        CurrentDeviceId = currentDeviceId;
    }

    // Âm lượng hiện tại (0..1), ưu tiên phiên đang phát
    public float GetVolume()
    {
        foreach (var s in Sessions)
        {
            try { if ((int)s.State == 1) return s.SimpleAudioVolume.Volume; } catch { }
        }
        foreach (var s in Sessions)
        {
            try { return s.SimpleAudioVolume.Volume; } catch { }
        }
        return 1f;
    }

    public bool GetMute()
    {
        foreach (var s in Sessions)
        {
            try { if ((int)s.State == 1) return s.SimpleAudioVolume.Mute; } catch { }
        }
        foreach (var s in Sessions)
        {
            try { return s.SimpleAudioVolume.Mute; } catch { }
        }
        return false;
    }

    // Đặt cho tất cả phiên của app (Chrome có nhiều phiên)
    public void SetVolume(float volume)
    {
        foreach (var s in Sessions)
        {
            try
            {
                // Chỉ set khi có chênh lệch đáng kể để tránh nghẽn COM khi kéo slider
                if (Math.Abs(s.SimpleAudioVolume.Volume - volume) > 0.005f)
                    s.SimpleAudioVolume.Volume = volume;
            }
            catch { }
        }
    }

    public void SetMute(bool mute)
    {
        foreach (var s in Sessions)
        {
            try { s.SimpleAudioVolume.Mute = mute; } catch { }
        }
    }
}

static class ProcessInfoCache
{
    private static readonly ConcurrentDictionary<uint, (string Name, string? ExePath)> _cache = new();

    [DllImport("kernel32.dll", SetLastError = true)]
    private static extern IntPtr OpenProcess(uint processAccess, bool bInheritHandle, uint processId);

    [DllImport("kernel32.dll", SetLastError = true, CharSet = CharSet.Unicode)]
    private static extern bool QueryFullProcessImageName(IntPtr hProcess, uint flags, [Out] StringBuilder lpExeName, ref uint lpdwSize);

    [DllImport("kernel32.dll", SetLastError = true)]
    [return: MarshalAs(UnmanagedType.Bool)]
    private static extern bool CloseHandle(IntPtr hObject);

    private const uint PROCESS_QUERY_LIMITED_INFORMATION = 0x1000;

    // Lấy tên và đường dẫn file exe siêu nhanh bằng Win32 API, không bị ném ngoại lệ phân quyền
    public static (string Name, string? ExePath) GetProcessInfo(uint pid)
    {
        if (_cache.TryGetValue(pid, out var cached))
            return cached;

        string? exePath = null;
        string name = "";

        IntPtr hProcess = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid);
        if (hProcess != IntPtr.Zero)
        {
            try
            {
                var sb = new StringBuilder(1024);
                uint size = (uint)sb.Capacity;
                if (QueryFullProcessImageName(hProcess, 0, sb, ref size))
                {
                    exePath = sb.ToString();
                    name = Path.GetFileNameWithoutExtension(exePath);
                }
            }
            finally
            {
                CloseHandle(hProcess);
            }
        }

        if (string.IsNullOrEmpty(name))
        {
            try
            {
                using var proc = Process.GetProcessById((int)pid);
                name = proc.ProcessName;
                if (exePath == null)
                {
                    try { exePath = proc.MainModule?.FileName; } catch { }
                }
            }
            catch
            {
                name = $"PID_{pid}";
            }
        }

        var result = (name, exePath);
        _cache[pid] = result;
        return result;
    }

    // Dọn dẹp cache của các tiến trình đã tắt để giải phóng bộ nhớ
    public static void Prune(HashSet<uint> activePids)
    {
        if (_cache.Count > activePids.Count + 10)
        {
            foreach (var key in _cache.Keys)
            {
                if (!activePids.Contains(key))
                    _cache.TryRemove(key, out _);
            }
        }
    }
}

static class AudioService
{
    // Gộp các tiến trình cùng tên (ví dụ Chrome có nhiều tiến trình) thành một dòng
    public static List<AppEntry> GetApps(List<MMDevice> devices)
    {
        var map = new Dictionary<string, AppEntry>(StringComparer.OrdinalIgnoreCase);
        var activePids = new HashSet<uint>();

        foreach (var device in devices)
        {
            SessionCollection? sessions = null;
            try
            {
                sessions = device.AudioSessionManager.Sessions;
            }
            catch
            {
                continue;
            }

            try
            {
                int count = sessions.Count;
                for (int i = 0; i < count; i++)
                {
                    AudioSessionControl session;
                    try
                    {
                        session = sessions[i];
                    }
                    catch { continue; }

                    uint pid = (uint)session.GetProcessID;
                    if (pid == 0)
                    {
                        try { session.Dispose(); } catch { }
                        continue; // bỏ qua âm thanh hệ thống
                    }

                    activePids.Add(pid);
                    var (name, exePath) = ProcessInfoCache.GetProcessInfo(pid);
                    if (string.IsNullOrEmpty(name))
                    {
                        try { session.Dispose(); } catch { }
                        continue;
                    }

                    if (!map.TryGetValue(name, out var entry))
                    {
                        entry = new AppEntry(name, device.ID);
                        map[name] = entry;
                    }
                    entry.Pids.Add(pid);
                    entry.Sessions.Add(session);

                    if (entry.ExePath == null && exePath != null)
                    {
                        entry.ExePath = exePath;
                    }

                    // Trạng thái 1 = đang phát. Ưu tiên thiết bị đang thật sự phát.
                    try
                    {
                        if ((int)session.State == 1) entry.CurrentDeviceId = device.ID;
                    }
                    catch { }
                }
            }
            finally
            {
                try { sessions.Dispose(); } catch { }
            }
        }

        ProcessInfoCache.Prune(activePids);
        return map.Values.OrderBy(a => a.Name, StringComparer.OrdinalIgnoreCase).ToList();
    }
}