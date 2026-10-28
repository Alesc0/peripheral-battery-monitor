use super::*;
use gtk::glib;

pub(super) fn run_app() -> Result<(), Box<dyn Error>> {
    gtk::init()?;
    let (ui_tx, ui_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let mut app = TrayApp::new(command_tx);
    app.create_tray()?;
    thread::Builder::new()
        .name("peripheral-battery-poll".into())
        .spawn(move || poll_worker(ui_tx, command_rx))?;

    // GTK owns the tray and menus. Only the polling worker touches HID.
    glib::timeout_add_local(Duration::from_millis(100), move || {
        while let Ok(UiEvent::Batteries(result)) = ui_rx.try_recv() {
            app.show_state(result);
        }
        if app.handle_menu_events() {
            gtk::main_quit();
            return glib::ControlFlow::Break;
        }
        glib::ControlFlow::Continue
    });
    gtk::main();
    Ok(())
}

fn xdg_directory(variable: &str, fallback: &str) -> std::io::Result<PathBuf> {
    if let Some(path) = std::env::var_os(variable)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
    {
        return Ok(path);
    }
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .map(|home| home.join(fallback))
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "HOME is not set to an absolute path",
            )
        })
}

fn config_directory() -> std::io::Result<PathBuf> {
    Ok(xdg_directory("XDG_CONFIG_HOME", ".config")?.join("peripheral-battery-monitor"))
}

fn state_directory() -> std::io::Result<PathBuf> {
    Ok(xdg_directory("XDG_STATE_HOME", ".local/state")?.join("peripheral-battery-monitor"))
}

fn write_file(path: &std::path::Path, contents: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, contents)?;
    fs::rename(temporary, path)
}

pub(super) fn device_alarm_enabled(device_id: &str) -> bool {
    config_directory()
        .and_then(|dir| fs::read_to_string(dir.join(alarm_registry_value_name(device_id))))
        .map(|value| value.trim() != "0")
        .unwrap_or(true)
}

pub(super) fn set_device_alarm_enabled(device_id: &str, enabled: bool) -> std::io::Result<()> {
    write_file(
        &config_directory()?.join(alarm_registry_value_name(device_id)),
        if enabled { "1\n" } else { "0\n" },
    )
}

pub(super) fn load_cached_battery(device_id: &str, device_name: &str) -> Option<Battery> {
    let path = state_directory()
        .ok()?
        .join(alarm_registry_value_name(device_id));
    let value = fs::read_to_string(path).ok()?;
    parse_cached_battery(&value, device_id, device_name)
}

fn parse_cached_battery(value: &str, device_id: &str, device_name: &str) -> Option<Battery> {
    let (level, charging) = value.trim().split_once(' ')?;
    let level = level.parse::<u8>().ok().filter(|level| *level <= 100)?;
    let charging = match charging {
        "1" => true,
        "0" => false,
        _ => return None,
    };
    Some(Battery {
        device_id: device_id.to_owned(),
        device_name: device_name.to_owned(),
        level,
        charging,
        is_stale: true,
    })
}

pub(super) fn store_cached_battery(battery: &Battery) -> std::io::Result<()> {
    write_file(
        &state_directory()?.join(alarm_registry_value_name(&battery.device_id)),
        &format!("{} {}\n", battery.level, u8::from(battery.charging)),
    )
}

fn autostart_path() -> std::io::Result<PathBuf> {
    Ok(xdg_directory("XDG_CONFIG_HOME", ".config")?
        .join("autostart/peripheral-battery-monitor.desktop"))
}

pub(super) fn startup_enabled() -> bool {
    autostart_path().is_ok_and(|path| path.is_file())
}

fn desktop_exec(executable: &str) -> String {
    let mut command = String::from("\"");
    for character in executable.chars() {
        match character {
            '%' => command.push_str("%%"),
            '\\' | '"' | '`' | '$' => {
                command.push('\\');
                command.push(character);
            }
            _ => command.push(character),
        }
    }
    command.push('"');
    command
}

pub(super) fn set_startup_enabled(enabled: bool) -> std::io::Result<()> {
    let path = autostart_path()?;
    if !enabled {
        return match fs::remove_file(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            result => result,
        };
    }
    let executable = std::env::current_exe()?;
    let executable = executable.to_str().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "executable path is not UTF-8",
        )
    })?;
    let entry = glib::KeyFile::new();
    entry.set_string("Desktop Entry", "Type", "Application");
    entry.set_string("Desktop Entry", "Name", "Peripheral Battery Monitor");
    entry.set_string("Desktop Entry", "Exec", &desktop_exec(executable));
    entry.set_boolean("Desktop Entry", "Terminal", false);
    write_file(&path, entry.to_data().as_str())
}

pub(super) fn play_low_battery_alarm() {
    // Reap the child off the UI thread. No shell or device values are involved.
    thread::spawn(|| {
        match std::process::Command::new("canberra-gtk-play")
            .args([
                "--id=dialog-warning",
                "--description=Low peripheral battery",
            ])
            .status()
        {
            Ok(status) if status.success() => {}
            Ok(status) => log_diagnostic(&format!("low-battery sound failed: {status}")),
            Err(error) => log_diagnostic(&format!("could not play low-battery sound: {error}")),
        }
    });
}

pub(super) fn diagnostic_log_path() -> Option<PathBuf> {
    state_directory()
        .ok()
        .map(|dir| dir.join("diagnostics.log"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_readings_are_stale_and_invalid_cache_is_ignored() {
        let battery = parse_cached_battery("7 1\n", "test-device", "ScyRox").unwrap();
        assert_eq!(battery.level, 7);
        assert!(battery.charging);
        assert!(battery.is_stale);
        assert_eq!(battery.device_id, "test-device");
        for invalid in ["101 0", "256 0", "7 2", "7", "bad 0", ""] {
            assert!(parse_cached_battery(invalid, "test-device", "ScyRox").is_none());
        }
    }

    #[test]
    fn desktop_entry_preserves_escaped_executable_path() {
        let path = "/home/user/a b/quote\"dollar$`slash\\percent%/monitor";
        let command = desktop_exec(path);
        let entry = glib::KeyFile::new();
        entry.set_string("Desktop Entry", "Exec", &command);
        let loaded = glib::KeyFile::new();
        loaded
            .load_from_data(&entry.to_data(), glib::KeyFileFlags::NONE)
            .unwrap();
        assert_eq!(loaded.string("Desktop Entry", "Exec").unwrap(), command);
        assert_eq!(
            command,
            "\"/home/user/a b/quote\\\"dollar\\$\\`slash\\\\percent%%/monitor\""
        );
    }
}
