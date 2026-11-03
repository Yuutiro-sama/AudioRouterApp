# Security Policy

## Supported Versions

| Version | Supported          |
| ------- | ------------------ |
| 1.0.x   | :white_check_mark: |

## Security & Privacy Highlights

**Audio Router App** is designed with privacy and security as top priorities:

1. **100% Offline & No Telemetry**:
   - The application does not collect, track, or transmit any user data, process names, or audio sessions over the network.
   - No outgoing or incoming network connections are initiated.

2. **Least Privilege Principle**:
   - The application does not require administrator privileges to operate.
   - Process inspection uses `PROCESS_QUERY_LIMITED_INFORMATION` via Win32 APIs, ensuring safe access without elevated rights.
   - Startup settings are stored under `HKEY_CURRENT_USER\Software\Microsoft\Windows\CurrentVersion\Run` rather than `HKEY_LOCAL_MACHINE`.

3. **Safe Local Storage**:
   - User configuration is stored locally at `%AppData%\AudioRouter\rules.json`.
   - No sensitive information (such as passwords, tokens, or personal identifiers) is stored.

4. **COM & Resource Cleanup**:
   - All COM interfaces (`IMMNotificationClient`, `IMMDeviceEnumerator`, `IAudioPolicyConfigFactoryVariantFor21H2`) and unmanaged memory handles are properly tracked and released upon application exit or device unplug.

## Reporting a Vulnerability

If you discover a security vulnerability within this project:
1. Please do not create a public issue on GitHub.
2. Send an email with details and reproduction steps to: `dangtanmin111@gmail.com`.
3. We will acknowledge receipt of your report within 48 hours and provide an estimated timeline for a fix.
