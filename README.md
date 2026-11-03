# Audio Router App 🎧

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)
[![Platform](https://img.shields.io/badge/Platform-Windows%2010%20%7C%2011-lightgrey.svg)](https://microsoft.com/windows)
[![.NET](https://img.shields.io/badge/.NET-10.0-purple.svg)](https://dotnet.microsoft.com/)

A lightweight, modern Windows desktop utility built with **WPF (.NET 10)** that allows you to route audio from specific applications to specific output devices (headphones, speakers, DACs, virtual cables) independently, adjust per-app volume, and automatically restore routing when devices are plugged or unplugged.

---

## 🌟 Key Features

- **Per-Application Audio Routing**: Direct sound from Chrome to headphones, Spotify to speakers, or a game to a specific DAC.
- **Smart Hardware Hotplug Detection**:
  - Automatically listens to Windows audio device changes (`IMMNotificationClient`).
  - Automatically re-applies routing rules when a headset or USB DAC is plugged back in.
- **Per-Application Volume & Mute**: Fine-tune volume or mute individual running applications directly from a single sleek tray popup.
- **System Tray Integration**:
  - Runs silently in the background with a minimal footprint.
  - One-click tray popup with fast response time.
- **Persistent Routing Rules**: Remembers your assignments in `%AppData%\AudioRouter\rules.json` and reapplies them whenever target apps launch.
- **Start with Windows**: Toggle autostart cleanly via user registry without requiring administrator privileges.
- **High Performance & Low Latency**:
  - Built-in caching for process metadata and application icons (`ProcessInfoCache`, `IconCache`).
  - Zero polling lag and low CPU/RAM usage.

---

## 🛡️ Security & Privacy

- **100% Offline**: No network requests, no telemetry, no tracking, no analytics.
- **Least Privilege**: Runs completely without Administrator rights (`PROCESS_QUERY_LIMITED_INFORMATION`).
- **No Sensitive Data**: Only stores application names and target audio device IDs locally on your machine.
- Read our [SECURITY.md](SECURITY.md) for more details.

---

## 📋 Requirements

- **Operating System**: Windows 10 (version 1809+ / 21H2+) or Windows 11 (64-bit).
- **Runtime**:
  - For standard build: [.NET 10 Desktop Runtime](https://dotnet.microsoft.com/download/dotnet/10.0)
  - For single-file self-contained build: No additional runtime required.

---

## 🛠️ How to Build from Source

### Prerequisites
- [.NET 10 SDK](https://dotnet.microsoft.com/download/dotnet/10.0)
- Visual Studio 2022+ / Visual Studio Community or VS Code with C# Dev Kit.

### 1. Clone the repository
```bash
git clone https://github.com/yuutiro/AudioRouterApp.git
cd AudioRouterApp
```

### 2. Build Debug
```bash
dotnet build
```

### 3. Publish Single-File Executable (Standalone)
To produce a single, self-contained `.exe` that runs anywhere on Windows without installing .NET:
```bash
dotnet publish AudioRouterApp/AudioRouterApp.csproj -c Release -r win-x64 --self-contained true -p:PublishSingleFile=true -p:IncludeNativeLibrariesForSelfExtract=true -p:EnableCompressionInSingleFile=true -o ./PublishSingleFile
```
The resulting executable will be in `./PublishSingleFile/AudioRouterApp.exe`.

---

## 🚀 How to Run

1. Run `AudioRouterApp.exe`.
2. Look for the blue speaker icon in your Windows Taskbar System Tray (near the clock).
3. Click the icon to view all active audio applications.
4. Select the desired audio output device from the dropdown menu for any app.

---

## 🏗️ Architecture Overview

- **Core Audio Routing**: Communicates with internal Windows Audio Policy via undocumented COM factory `Windows.Media.Internal.AudioPolicyConfig` (`SetPersistedDefaultAudioEndpoint`).
- **Audio Sessions**: Managed via `NAudio.CoreAudioApi` (`MMDeviceEnumerator`, `AudioSessionManager`).
- **Device State Monitoring**: Custom COM implementation of `IMMNotificationClient` registered with Windows Core Audio MMDevAPI.

---

## 📄 License

This project is licensed under the [MIT License](LICENSE).
