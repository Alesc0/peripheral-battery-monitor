# Peripheral Battery Monitor

Peripheral Battery Monitor is a small Windows system tray app for checking peripheral battery levels. It currently supports ScyRox wireless mice, with a general app name and structure for adding more devices later.

The app can poll multiple supported devices. Under **Settings > Low battery alarms**, toggle the warning sound for each detected device. An enabled alarm sounds once when that device reaches 10% or less, and rearms after it rises above 10%.

HID diagnostics are written to `%LOCALAPPDATA%\PeripheralBatteryMonitor\diagnostics.log` and rotated when the file reaches 1 MB.

Click the tray icon to see the connected device name, battery percentage, and charging state. The app refreshes every five seconds. Use **Settings → Start with Windows** to toggle startup for your Windows account, **Refresh battery** to request an immediate update, or **Quit** to close the app.

## Download the portable executable

Every push to `main` builds a Windows x64 executable. To download it:

1. Open the repository's **Actions** tab.
2. Open the latest successful **Build Windows portable executable** run.
3. Download the `peripheral-battery-monitor-windows-x64` artifact and extract it.
4. Run `peripheral-battery-monitor.exe`.

The executable statically links the MSVC C runtime, so it does not require a separate Visual C++ Redistributable installer. It is a standalone `.exe`, not an installer. The workflow artifact is retained for 90 days.

## Build locally

Install Rust and the Visual Studio C++ Build Tools, then run these commands in PowerShell:

```powershell
cd C:\path\to\peripheral-battery-monitor
cargo build --release --locked --target x86_64-pc-windows-msvc
```

The executable is written to `target\x86_64-pc-windows-msvc\release\peripheral-battery-monitor.exe`. To run the app directly during development, use `cargo run --release`.

## Device support

Current support is ScyRox VID `3554`, wireless products `F5F4`, `F5F7`, and `F517`. Wired models do not report battery. If no supported receiver is available or the mouse is asleep, the tray tooltip reports the current state.

The app reads report ID 8, command 3 for receiver online state, and command 4 for battery level and charging state. It displays the raw level reported by the receiver; ScyRox's official app applies voltage-based conversion and smoothing, so the displayed percentage can differ. The app sends read commands only and does not modify mouse settings.

The startup toggle writes a per-user Windows startup entry that points to the executable's current path. Keep the executable in a stable folder; if you move it, turn startup off and back on to update the path. Avoid polling the same receiver with another app at the same time, as their HID requests may interfere.
