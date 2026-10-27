# Repository guidance

## Project

Peripheral Battery Monitor is a Rust Windows system-tray application. The current device protocol supports ScyRox wireless mice (VID `3554`, products `F5F4`, `F5F7`, and `F517`). Most application code is in `src/main.rs`; the Windows x64 portable build is defined in `.github/workflows/build-windows.yml`.

## Making changes

- Preserve the tray app's asynchronous polling model: HID work runs on the polling worker and UI/menu updates are sent back through `UiEvent`.
- Keep device discovery and protocol handling in the HID polling/query code. Filter interfaces carefully; a receiver can expose multiple HID collections, and duplicate reads can cause access errors.
- Treat sleeping or offline devices as normal. Continue showing cached battery values as stale, and do not fire low-battery alarms from stale readings.
- Keep per-user Windows settings and battery cache under `HKCU\Software\PeripheralBatteryMonitor`. Do not put raw HID serials or device paths into registry value names; use the existing hashed identity helper.
- Keep HID diagnostics in `%LOCALAPPDATA%\PeripheralBatteryMonitor\diagnostics.log`, with bounded log size and useful context around failures. Avoid logging sensitive values.
- The app currently uses Windows-specific HID, registry, startup, and sound behavior. Guard platform-specific code appropriately if adding cross-platform support, and update the README to describe actual support.
- Keep the README aligned with the supported products, behavior, startup requirements, diagnostics path, and artifact download/build instructions.

## Build

Use the locked dependency graph for reproducible builds. The CI workflow builds the portable Windows x64 executable with:

```powershell
cargo build --release --locked --target x86_64-pc-windows-msvc
```

The CI artifact is `target/x86_64-pc-windows-msvc/release/peripheral-battery-monitor.exe`.

## Changes and verification

- Keep changes focused and avoid committing generated `target/` files.
- When asked to verify a change, use the relevant Cargo build/check command and report any platform/toolchain limitations. Windows HID behavior needs validation on Windows with a supported receiver.
- For HID failures, inspect the diagnostic log and include the product ID, usage page, usage, and interface in troubleshooting output where available.
