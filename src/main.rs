#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use hidapi::{HidApi, HidDevice, HidError};
use std::{
    collections::{HashMap, HashSet},
    error::Error,
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant},
};
use tray_icon::{
    Icon, TrayIcon, TrayIconBuilder,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, Submenu},
};
use winit::{
    application::ApplicationHandler,
    event_loop::{ActiveEventLoop, ControlFlow, EventLoop, EventLoopProxy},
};
use winreg::{RegKey, enums::HKEY_CURRENT_USER};

const VENDOR_ID: u16 = 0x3554;
const WIRELESS_PRODUCT_IDS: [u16; 3] = [0xf5f4, 0xf5f7, 0xf517];
const HID_REPORT_ID: u8 = 8;
const POLL_INTERVAL: Duration = Duration::from_secs(5);
const LOW_BATTERY_THRESHOLD: u8 = 10;
const ACCESS_DENIED_RETRY_DELAYS: [Duration; 2] = [
    Duration::from_millis(250),
    Duration::from_millis(750),
];
const RESPONSE_TIMEOUT: Duration = Duration::from_millis(1_200);
const STARTUP_RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const STARTUP_VALUE: &str = "PeripheralBatteryMonitor";
const ALARM_SETTINGS_KEY: &str = r"Software\PeripheralBatteryMonitor\BatteryAlarms";

enum UiEvent {
    Batteries(Result<Vec<Battery>, String>),
}

enum Command {
    Refresh,
}

struct Battery {
    device_id: String,
    device_name: String,
    level: u8,
    charging: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();
    let (command_tx, command_rx) = mpsc::channel();
    thread::Builder::new()
        .name("peripheral-battery-poll".into())
        .spawn(move || poll_worker(proxy, command_rx))?;

    let mut app = TrayApp::new(command_tx);
    event_loop.run_app(&mut app)?;
    Ok(())
}

struct TrayApp {
    command_tx: Sender<Command>,
    tray: Option<TrayIcon>,
    status_item: Option<MenuItem>,
    refresh_item: Option<MenuItem>,
    startup_item: Option<CheckMenuItem>,
    alarm_submenu: Option<Submenu>,
    alarm_items: HashMap<String, CheckMenuItem>,
    low_battery_alarm_fired: HashSet<String>,
    next_alarm_menu_id: u32,
    quit_item: Option<MenuItem>,
}

impl TrayApp {
    fn new(command_tx: Sender<Command>) -> Self {
        Self {
            command_tx,
            tray: None,
            status_item: None,
            refresh_item: None,
            startup_item: None,
            alarm_submenu: None,
            alarm_items: HashMap::new(),
            low_battery_alarm_fired: HashSet::new(),
            next_alarm_menu_id: 0,
            quit_item: None,
        }
    }

    fn create_tray(&mut self) -> Result<(), Box<dyn Error>> {
        if self.tray.is_some() {
            return Ok(());
        }

        let status = MenuItem::with_id("battery-status", "Searching for a supported device…", false, None);
        let refresh = MenuItem::with_id("refresh-battery", "Refresh battery", true, None);
        let startup = CheckMenuItem::with_id(
            "windows-startup",
            "Start with Windows",
            true,
            startup_enabled(),
            None,
        );
        let alarms = Submenu::new("Low battery alarms", true);
        let settings = Submenu::with_items("Settings", true, &[&startup, &alarms])?;
        let quit = MenuItem::with_id("quit-app", "Quit", true, None);
        let menu = Menu::with_items(&[&status, &refresh, &settings, &quit])?;
        let tray = TrayIconBuilder::new()
            .with_tooltip("Peripheral Battery: searching…")
            .with_icon(draw_battery_icon(None))
            .with_menu(Box::new(menu))
            .build()?;
        tray.set_show_menu_on_left_click(true);

        self.status_item = Some(status);
        self.refresh_item = Some(refresh);
        self.startup_item = Some(startup);
        self.alarm_submenu = Some(alarms);
        self.quit_item = Some(quit);
        self.tray = Some(tray);
        Ok(())
    }

    fn show_state(&mut self, result: Result<Vec<Battery>, String>) {
        let (icon, tooltip, menu_text) = match result {
            Ok(batteries) if !batteries.is_empty() => {
                for battery in &batteries {
                    self.register_device_alarm(battery);
                    self.update_low_battery_alarm(battery);
                }

                let lowest = batteries.iter().min_by_key(|battery| battery.level).unwrap();
                let details = batteries
                    .iter()
                    .map(|battery| {
                        let charge = if battery.charging { " · charging" } else { "" };
                        format!("{}: {}%{}", battery.device_name, battery.level, charge)
                    })
                    .collect::<Vec<_>>();
                (
                    draw_battery_icon(Some(lowest.level)),
                    details.join(" | "),
                    details.join(" | "),
                )
            }
            Ok(_) => (
                draw_battery_icon(None),
                "Peripheral Battery: no supported device found".into(),
                "no supported device found".into(),
            ),
            Err(message) => (
                draw_battery_icon(None),
                format!("Peripheral Battery: {message}"),
                message,
            ),
        };

        if let Some(tray) = &self.tray {
            let _ = tray.set_icon(Some(icon));
            let _ = tray.set_tooltip(Some(tooltip));
        }
        if let Some(item) = &self.status_item {
            item.set_text(menu_text);
        }
    }

    fn register_device_alarm(&mut self, battery: &Battery) {
        if self.alarm_items.contains_key(&battery.device_id) {
            return;
        }

        let Some(submenu) = &self.alarm_submenu else {
            return;
        };

        self.next_alarm_menu_id += 1;
        let menu_id = format!("battery-alarm-{}", self.next_alarm_menu_id);
        let item = CheckMenuItem::with_id(
            menu_id,
            format!("{} low-battery alarm", battery.device_name),
            true,
            device_alarm_enabled(&battery.device_id),
            None,
        );
        if submenu.append(&item).is_ok() {
            self.alarm_items.insert(battery.device_id.clone(), item);
        }
    }

    fn update_low_battery_alarm(&mut self, battery: &Battery) {
        if battery.level > LOW_BATTERY_THRESHOLD {
            self.low_battery_alarm_fired.remove(&battery.device_id);
            return;
        }

        let first_low_reading = self.low_battery_alarm_fired.insert(battery.device_id.clone());
        let enabled = self
            .alarm_items
            .get(&battery.device_id)
            .is_some_and(CheckMenuItem::is_checked);
        if first_low_reading && enabled {
            play_low_battery_alarm();
        }
    }
}

impl ApplicationHandler<UiEvent> for TrayApp {
    fn window_event(
        &mut self,
        _event_loop: &ActiveEventLoop,
        _window_id: winit::window::WindowId,
        _event: winit::event::WindowEvent,
    ) {
    }

    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if let Err(error) = self.create_tray() {
            eprintln!("Could not create the Windows tray icon: {error}");
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Batteries(result) => self.show_state(result),
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            if self.refresh_item.as_ref().is_some_and(|item| &event.id == item.id()) {
                let _ = self.command_tx.send(Command::Refresh);
            } else if self.startup_item.as_ref().is_some_and(|item| &event.id == item.id()) {
                if let Some(item) = &self.startup_item {
                    let enabled = item.is_checked();
                    if let Err(error) = set_startup_enabled(enabled) {
                        item.set_checked(!enabled);
                        if let Some(status) = &self.status_item {
                            status.set_text(format!("Could not update startup setting: {error}"));
                        }
                    }
                }
            } else if let Some(device_id) = self
                .alarm_items
                .iter()
                .find(|(_, item)| &event.id == item.id())
                .map(|(device_id, _)| device_id.clone())
            {
                if let Some(item) = self.alarm_items.get(&device_id) {
                    let enabled = item.is_checked();
                    if let Err(error) = set_device_alarm_enabled(&device_id, enabled) {
                        item.set_checked(!enabled);
                        if let Some(status) = &self.status_item {
                            status.set_text(format!("Could not save alarm setting: {error}"));
                        }
                    }
                }
            } else if self.quit_item.as_ref().is_some_and(|item| &event.id == item.id()) {
                event_loop.exit();
            }
        }
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(250)));
    }
}

fn startup_enabled() -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey(STARTUP_RUN_KEY)
        .and_then(|run_key| run_key.get_value::<String, _>(STARTUP_VALUE))
        .is_ok()
}

fn device_alarm_enabled(device_id: &str) -> bool {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    hkcu.open_subkey(ALARM_SETTINGS_KEY)
        .and_then(|key| key.get_value::<u32, _>(&alarm_registry_value_name(device_id)))
        .map(|value| value != 0)
        .unwrap_or(true)
}

fn set_device_alarm_enabled(device_id: &str, enabled: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    let (key, _) = hkcu.create_subkey(ALARM_SETTINGS_KEY)?;
    key.set_value(&alarm_registry_value_name(device_id), &u32::from(enabled))
}

fn alarm_registry_value_name(device_id: &str) -> String {
    // Keep arbitrary HID serials/paths out of registry value names.
    let hash = device_id.as_bytes().iter().fold(0xcbf29ce484222325u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    });
    format!("device_{hash:016x}")
}

#[link(name = "user32")]
unsafe extern "system" {
    fn MessageBeep(beep_type: u32) -> i32;
}

fn play_low_battery_alarm() {
    const MB_ICONWARNING: u32 = 0x30;
    // Ask Windows to play the configured warning sound (or its default beep).
    unsafe {
        MessageBeep(MB_ICONWARNING);
    }
}

fn set_startup_enabled(enabled: bool) -> std::io::Result<()> {
    let hkcu = RegKey::predef(HKEY_CURRENT_USER);
    if enabled {
        let (run_key, _) = hkcu.create_subkey(STARTUP_RUN_KEY)?;
        let executable = std::env::current_exe()?;
        run_key.set_value(STARTUP_VALUE, &format!("\"{}\"", executable.display()))
    } else {
        match hkcu.open_subkey(STARTUP_RUN_KEY) {
            Ok(run_key) => match run_key.delete_value(STARTUP_VALUE) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }
}

fn poll_worker(proxy: EventLoopProxy<UiEvent>, commands: Receiver<Command>) {
    let mut first_poll = true;
    loop {
        if !first_poll {
            match commands.recv_timeout(POLL_INTERVAL) {
                Ok(Command::Refresh) | Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return,
            }
        }
        first_poll = false;

        let result = read_batteries_with_retry();
        let _ = proxy.send_event(UiEvent::Batteries(result));
    }
}

fn read_batteries_with_retry() -> Result<Vec<Battery>, String> {
    for delay in ACCESS_DENIED_RETRY_DELAYS {
        match read_batteries() {
            Err(message) if message.contains("[ACCESS_DENIED]") => thread::sleep(delay),
            result => return result,
        }
    }

    read_batteries().map_err(|message| {
        if message.contains("[ACCESS_DENIED]") {
            "Windows denied access to the mouse receiver. Close ScyRox or other mouse software, then choose Refresh battery."
                .to_string()
        } else {
            message
        }
    })
}

fn read_batteries() -> Result<Vec<Battery>, String> {
    let api = HidApi::new().map_err(|error| format!("HID unavailable: {error}"))?;
    let mut last_error = "wireless mouse not found".to_string();
    let mut batteries = Vec::new();
    let mut connected_device_ids = HashSet::new();

    for info in api.device_list() {
        if info.vendor_id() != VENDOR_ID || !WIRELESS_PRODUCT_IDS.contains(&info.product_id()) {
            continue;
        }
        // Composite receivers also expose keyboard/mouse system collections.
        // Windows may open those read-only, so sending vendor commands to them
        // produces misleading access-denied errors. The ScyRox protocol lives
        // on its vendor-defined HID collections.
        if info.usage_page() < 0xff00 {
            continue;
        }
        let device_id = device_identity(&info);
        if connected_device_ids.contains(&device_id) {
            continue;
        }

        let device = match api.open_path(info.path()) {
            Ok(device) => device,
            Err(error) => {
                last_error = format!(
                    "{} (HID page {:04x}, interface {})",
                    format_hid_error("could not open vendor receiver", error),
                    info.usage_page(),
                    info.interface_number()
                );
                continue;
            }
        };

        let device_name = info
            .product_string()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or("ScyRox wireless mouse")
            .to_owned();
        match query_device(&device, device_name, device_id.clone()) {
            Ok(battery) => {
                connected_device_ids.insert(device_id);
                batteries.push(battery);
            }
            Err(error) => {
                last_error = format!(
                    "{error} (HID page {:04x}, interface {})",
                    info.usage_page(),
                    info.interface_number()
                )
            }
        }
    }

    if batteries.is_empty() {
        Err(last_error)
    } else {
        Ok(batteries)
    }
}

fn device_identity(info: &hidapi::DeviceInfo) -> String {
    let serial = info.serial_number().filter(|serial| !serial.trim().is_empty());
    let identity = if let Some(serial) = serial {
        format!("serial:{}", serial.trim())
    } else {
        let mut path = info.path().to_string_lossy().to_ascii_lowercase();
        for collection in 1..=16 {
            path = path.replace(&format!("&col{collection:02x}"), "&colxx");
            path = path.replace(&format!("&col{collection:02}"), "&colxx");
        }
        format!("path:{path}")
    };
    format!("{:04x}:{}", info.product_id(), identity)
}

fn query_device(device: &HidDevice, device_name: String, device_id: String) -> Result<Battery, String> {
    // The ScyRox app initializes this HID endpoint with an eight-byte nonce.
    let mut init_data = [0u8; 8];
    getrandom::fill(&mut init_data).map_err(|error| format!("random initializer failed: {error}"))?;
    request(device, 1, Some(&init_data))?;

    let online = request(device, 3, None)?;
    if online.get(5).copied() != Some(1) {
        return Err("mouse is offline; wake it and reconnect".into());
    }

    let response = request(device, 4, None)?;
    let level = response.get(5).copied().ok_or("short battery response")?.min(100);
    let charging = response.get(6).copied().ok_or("short battery response")? == 1;
    Ok(Battery { device_id, device_name, level, charging })
}

fn request(device: &HidDevice, command: u8, data: Option<&[u8]>) -> Result<Vec<u8>, String> {
    let mut report = [0u8; 17];
    report[0] = HID_REPORT_ID;
    report[1] = command;
    if let Some(data) = data {
        if data.len() > 10 {
            return Err("invalid protocol request length".into());
        }
        report[5] = data.len() as u8;
        report[6..6 + data.len()].copy_from_slice(data);
    }
    let checksum = 85u8.wrapping_sub(report[1..16].iter().fold(0u8, |sum, byte| sum.wrapping_add(*byte)));
    report[16] = checksum.wrapping_sub(HID_REPORT_ID);

    device.write(&report).map_err(|error| format_hid_error("HID write failed", error))?;
    let started = Instant::now();
    let mut input = [0u8; 64];
    while started.elapsed() < RESPONSE_TIMEOUT {
        let remaining = RESPONSE_TIMEOUT.saturating_sub(started.elapsed()).as_millis() as i32;
        let count = device.read_timeout(&mut input, remaining.max(1))
            .map_err(|error| format_hid_error("HID read failed", error))?;
        if count >= 3 && input[0] == HID_REPORT_ID && input[1] == command && input[2] == 0 {
            return Ok(input[1..count].to_vec());
        }
    }
    Err(format!("Device command {command} timed out"))
}

fn format_hid_error(context: &str, error: HidError) -> String {
    let access_denied = matches!(
        &error,
        HidError::IoError { error } if error.raw_os_error() == Some(5)
    ) || error.to_string().to_ascii_lowercase().contains("access is denied");

    if access_denied {
        format!("{context}: access is denied [ACCESS_DENIED]")
    } else {
        format!("{context}: {error}")
    }
}

fn draw_battery_icon(level: Option<u8>) -> Icon {
    const SIZE: usize = 32;
    let mut pixels = vec![0u8; SIZE * SIZE * 4];
    let frame = [220, 224, 230, 255];
    let fill = match level {
        Some(value) if value <= 15 => [237, 91, 91, 255],
        Some(value) if value <= 35 => [238, 183, 78, 255],
        Some(_) => [159, 207, 93, 255],
        None => [112, 116, 126, 255],
    };

    // Battery outline and terminal.
    for y in 5..28 {
        for x in 7..25 {
            if x == 7 || x == 24 || y == 5 || y == 27 {
                pixel(&mut pixels, SIZE, x, y, frame);
            }
        }
    }
    for y in 2..5 {
        for x in 12..20 {
            pixel(&mut pixels, SIZE, x, y, frame);
        }
    }

    if let Some(value) = level {
        let rows = value as usize * 17 / 100;
        for y in (9..26).rev().take(rows) {
            for x in 10..22 {
                pixel(&mut pixels, SIZE, x, y, fill);
            }
        }
    } else {
        for y in 12..21 {
            for x in 10..22 {
                if (x + y) % 4 == 0 {
                    pixel(&mut pixels, SIZE, x, y, fill);
                }
            }
        }
    }

    Icon::from_rgba(pixels, SIZE as u32, SIZE as u32).expect("valid 32x32 icon")
}

fn pixel(pixels: &mut [u8], width: usize, x: usize, y: usize, color: [u8; 4]) {
    let index = (y * width + x) * 4;
    pixels[index..index + 4].copy_from_slice(&color);
}
