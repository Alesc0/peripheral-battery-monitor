# Peripheral Battery Monitor

Peripheral Battery Monitor is a small Windows and Linux system tray app for checking peripheral battery levels. It currently supports ScyRox wireless mice.

The app can poll multiple supported devices. Under **Settings > Low battery alarms**, toggle the warning sound for each detected device. An enabled alarm sounds once when that device reaches 10% or less, and rearms after it rises above 10%.

The latest battery level is saved per device. If the mouse sleeps or stops responding while its receiver is still connected, the tray shows the last known percentage and marks it as stale.

HID diagnostics rotate at 1 MB. Windows writes them to `%LOCALAPPDATA%\PeripheralBatteryMonitor\diagnostics.log`. Linux uses `$XDG_STATE_HOME/peripheral-battery-monitor/diagnostics.log`, or `~/.local/state/peripheral-battery-monitor/diagnostics.log` when that variable is unset.

Click the tray icon to see the connected device name, battery percentage, and charging state. The app refreshes every five seconds. Use **Settings > Start with Windows** on Windows or **Settings > Start at login** on Linux to toggle startup for your account, **Refresh battery** to request an immediate update, or **Quit** to close the app.

## Download a build

Every push to `main` builds Windows x64 and Linux x64 executables. To download one:

1. Open the repository's **Actions** tab.
2. Open the latest successful **Build Windows portable executable** or **Build Linux executable** run.
3. Download `peripheral-battery-monitor-windows-x64` or `peripheral-battery-monitor-linux-x64` and extract it.
4. On Windows, run `peripheral-battery-monitor.exe`. For Linux, follow the setup below and run `chmod +x` on the extracted executable.

The Windows executable statically links the MSVC C runtime, so it does not require a separate Visual C++ Redistributable installer. It is a standalone `.exe`, not an installer. The workflow artifact is retained for 90 days.

## Build on Windows

Install Rust and the Visual Studio C++ Build Tools, then run these commands in PowerShell:

```powershell
cd C:\path\to\peripheral-battery-monitor
cargo build --release --locked --target x86_64-pc-windows-msvc
```

The executable is written to `target\x86_64-pc-windows-msvc\release\peripheral-battery-monitor.exe`. To run the app directly during development, use `cargo run --release`.

## Linux setup and build

Linux support uses a StatusNotifier tray with hover tooltips and GTK 3 for the event loop and menus. On CachyOS or Arch, install the build and runtime dependencies:

```sh
sudo pacman -S --needed rust base-devel pkgconf gtk3 xdotool libcanberra
```

Run the app in your graphical desktop session. KDE Plasma provides a compatible system tray. GNOME needs an AppIndicator extension. Hover over the tray icon to see the battery percentage, charging state, and whether the reading is stale. You can also open the tray menu to read these details. Low-battery sounds use `canberra-gtk-play` from `libcanberra` and your desktop's sound theme.

Install the included receiver permission rules once:

```sh
sudo install -Dm644 packaging/70-peripheral-battery-monitor.rules /etc/udev/rules.d/70-peripheral-battery-monitor.rules
sudo udevadm control --reload-rules
```

Unplug and reconnect the receiver after installing the rules. These rules grant the active local user access only to the supported receivers. Run the app as your normal user.

Build and run from the repository:

```sh
cargo build --release --locked --target x86_64-unknown-linux-gnu
./target/x86_64-unknown-linux-gnu/release/peripheral-battery-monitor
```

The downloaded Linux artifact contains the executable and `packaging/70-peripheral-battery-monitor.rules`. Run the install command from the extracted artifact folder. The CI build runs on Ubuntu 24.04 and dynamically links system libraries; it requires glibc 2.39 or newer and the runtime packages listed above. CachyOS can also build directly from source with its installed libraries.

Linux saves alarm settings under `$XDG_CONFIG_HOME/peripheral-battery-monitor`, defaulting to `~/.config/peripheral-battery-monitor`. Battery cache and diagnostics go under `$XDG_STATE_HOME/peripheral-battery-monitor`, defaulting to `~/.local/state/peripheral-battery-monitor`. Per-device filenames use hashed identities rather than raw HID serials or paths. **Start at login** creates `$XDG_CONFIG_HOME/autostart/peripheral-battery-monitor.desktop`, defaulting to `~/.config/autostart/peripheral-battery-monitor.desktop`.

## Device support

Current support is ScyRox VID `3554`, wireless products `F5F4`, `F5F7`, and `F517`. Wired models do not report battery. If no supported receiver is available or the mouse is asleep, the tray tooltip reports the current state.

The app reads report ID 8, command 3 for receiver online state, and command 4 for battery level and charging state. It displays the raw level reported by the receiver; ScyRox's official app applies voltage-based conversion and smoothing, so the displayed percentage can differ. The app sends read commands only and does not modify mouse settings.

On Windows, the startup toggle writes a per-user Windows startup entry that points to the executable's current path. Windows alarm settings and battery cache stay under `HKCU\Software\PeripheralBatteryMonitor`. On Linux, the autostart entry also points to the current executable. Keep the executable in a stable folder; if you move it, turn startup off and back on to update the path. Avoid polling the same receiver with another app at the same time, as their HID requests may interfere.
