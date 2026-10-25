# Peripheral Battery Monitor

A small Windows tray app for battery monitoring across peripherals. It currently supports ScyRox wireless mice; the app name and structure are general so support for other devices can be added later. Click the tray icon to see the device name, battery percentage, and charging state. Use **Settings → Start with Windows** to toggle automatic startup for your Windows account, or choose **Refresh battery** and **Quit** from the menu.

## Build and run

Install the Rust toolchain, then run these commands in PowerShell:

```powershell
cd C:\Users\alex\Desktop\scyrox-dpi-tester\battery-tray
cargo run --release
```

The app checks every five seconds. Current device support is ScyRox VID `3554`, wireless products `F5F4`, `F5F7`, and `F517`. Wired models do not report battery. If a supported receiver is not found or the mouse is asleep, the tray tooltip explains the current state.

## Protocol basis and limits

The bundled ScyRox web app identifies report ID 8, command 3 for receiver online state, and command 4 for battery level, charging state, and voltage. This app sends those read commands only; it does not write mouse settings. It displays the raw level reported by the receiver, without ScyRox's battery smoothing. The device command handling has not yet been verified against a physical mouse, and another app using the same receiver at the same time may interfere with polling.
