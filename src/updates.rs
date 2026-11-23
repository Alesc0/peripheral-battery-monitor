use super::*;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read},
    path::Path,
};
use tempfile::TempDir;

const RELEASES_API: &str =
    "https://api.github.com/repos/Alesc0/peripheral-battery-monitor/releases/latest";
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
const MAX_DOWNLOAD_SIZE: u64 = 128 * 1024 * 1024;
#[cfg(windows)]
const ASSET_NAME: &str = "peripheral-battery-monitor.exe";
#[cfg(target_os = "linux")]
const ASSET_NAME: &str = "peripheral-battery-monitor-linux-x64";
const STAGING_PREFIX: &str = ".pbm-update-";

pub(super) fn enabled() -> bool {
    #[cfg(windows)]
    {
        RegKey::predef(HKEY_CURRENT_USER)
            .open_subkey(r"Software\PeripheralBatteryMonitor")
            .and_then(|key| key.get_value::<u32, _>("AutomaticUpdates"))
            .map(|value| value != 0)
            .unwrap_or(true)
    }
    #[cfg(target_os = "linux")]
    {
        linux::config_directory()
            .and_then(|dir| fs::read_to_string(dir.join("automatic-updates")))
            .map(|value| value.trim() != "0")
            .unwrap_or(true)
    }
}

pub(super) fn set_enabled(enabled: bool) -> std::io::Result<()> {
    #[cfg(windows)]
    {
        let (key, _) = RegKey::predef(HKEY_CURRENT_USER)
            .create_subkey(r"Software\PeripheralBatteryMonitor")?;
        key.set_value("AutomaticUpdates", &u32::from(enabled))
    }
    #[cfg(target_os = "linux")]
    {
        linux::write_file(
            &linux::config_directory()?.join("automatic-updates"),
            if enabled { "1\n" } else { "0\n" },
        )
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}

#[derive(Deserialize)]
struct Asset {
    name: String,
    size: u64,
    digest: Option<String>,
}

fn newer_build(release: &Release, current: u64) -> Option<u64> {
    if release.draft || release.prerelease || !release.assets.iter().any(|a| a.name == ASSET_NAME) {
        return None;
    }
    let build = release
        .tag_name
        .strip_prefix("build-")?
        .parse::<u64>()
        .ok()?;
    (build > current).then_some(build)
}

fn agent(timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(timeout))
        .build()
        .into()
}

fn check_for_update() -> Result<Option<(u64, Asset)>, Box<dyn Error>> {
    let mut response = match agent(Duration::from_secs(20))
        .get(RELEASES_API)
        .header(
            "User-Agent",
            concat!("PeripheralBatteryMonitor/", env!("CARGO_PKG_VERSION")),
        )
        .header("Accept", "application/vnd.github+json")
        .call()
    {
        Err(ureq::Error::StatusCode(404)) => return Ok(None),
        result => result?,
    };
    let release: Release = response.body_mut().read_json()?;
    let current = option_env!("PBM_BUILD_NUMBER")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let Some(build) = newer_build(&release, current) else {
        return Ok(None);
    };
    Ok(release
        .assets
        .into_iter()
        .find(|a| a.name == ASSET_NAME)
        .map(|a| (build, a)))
}

fn verify_download(
    reader: &mut impl Read,
    output: &mut impl Write,
    asset: &Asset,
    mut allowed: impl FnMut() -> bool,
) -> Result<(), Box<dyn Error>> {
    if asset.size == 0 || asset.size > MAX_DOWNLOAD_SIZE {
        return Err("update size is outside the allowed range".into());
    }
    let expected = asset
        .digest
        .as_deref()
        .and_then(|value| value.strip_prefix("sha256:"))
        .filter(|value| value.len() == 64 && value.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or("release asset has no valid SHA-256 digest")?;
    let mut hash = Sha256::new();
    let mut received = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        if !allowed() {
            return Err("automatic update cancelled".into());
        }
        let count = reader.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        received += count as u64;
        if received > asset.size {
            return Err("update exceeds its advertised size".into());
        }
        hash.update(&buffer[..count]);
        output.write_all(&buffer[..count])?;
    }
    if received != asset.size || !format!("{:x}", hash.finalize()).eq_ignore_ascii_case(expected) {
        return Err("update size or SHA-256 digest did not match".into());
    }
    Ok(())
}

pub(super) struct PreparedUpdate {
    staging: TempDir,
    target: PathBuf,
}

fn prepare_update(build: u64, asset: &Asset) -> Result<PreparedUpdate, Box<dyn Error>> {
    let target = std::env::current_exe()?;
    let staging = tempfile::Builder::new().prefix(STAGING_PREFIX).tempdir_in(
        target
            .parent()
            .ok_or("executable has no parent directory")?,
    )?;
    let path = staging.path().join("new-executable.exe");
    // Construct the URL from a numeric tag and a fixed asset name, never API-supplied URLs.
    let url = format!(
        "https://github.com/Alesc0/peripheral-battery-monitor/releases/download/build-{build}/{ASSET_NAME}"
    );
    let mut response = agent(Duration::from_secs(5 * 60))
        .get(&url)
        .header("User-Agent", "PeripheralBatteryMonitor")
        .call()?;
    let mut output = fs::File::create(&path)?;
    verify_download(
        &mut response.body_mut().as_reader(),
        &mut output,
        asset,
        enabled,
    )?;
    output.sync_all()?;
    drop(output);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    }
    verify_executable(&path)?;
    // Keep the previous executable available until the replacement has launched.
    fs::copy(&target, staging.path().join("previous-executable"))?;
    #[cfg(windows)]
    fs::copy(&target, staging.path().join("helper.exe"))?;
    Ok(PreparedUpdate { staging, target })
}

fn verify_executable(path: &Path) -> io::Result<()> {
    let mut child = std::process::Command::new(path)
        .arg("--update-health-check")
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                Ok(())
            } else {
                Err(io::Error::other(
                    "downloaded executable failed its startup check",
                ))
            };
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::other(
                "downloaded executable startup check timed out",
            ));
        }
        thread::sleep(Duration::from_millis(50));
    }
}

fn send_event(proxy: &UiEventSender, event: UiEvent) -> bool {
    #[cfg(windows)]
    {
        proxy.send_event(event).is_ok()
    }
    #[cfg(target_os = "linux")]
    {
        proxy.send(event).is_ok()
    }
}

pub(super) fn start_worker(proxy: UiEventSender) -> io::Result<Sender<bool>> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("peripheral-update-check".into())
        .spawn(move || {
            let mut automatic = enabled();
            let mut next_check = if std::env::args_os().any(|a| a == "--update-retry-later") {
                Instant::now() + CHECK_INTERVAL
            } else {
                Instant::now()
            };
            loop {
                if automatic && Instant::now() >= next_check {
                    let result = (|| -> Result<(), Box<dyn Error>> {
                        if !send_event(
                            &proxy,
                            UiEvent::UpdateStatus("Checking for updates…".into()),
                        ) {
                            return Ok(());
                        }
                        if let Some((build, asset)) = check_for_update()? {
                            if !enabled() {
                                return Ok(());
                            }
                            send_event(
                                &proxy,
                                UiEvent::UpdateStatus(format!("Downloading update {build}…")),
                            );
                            let prepared = prepare_update(build, &asset)?;
                            // The UI owns the final toggle check and restart decision.
                            send_event(&proxy, UiEvent::UpdateReady(prepared));
                        } else {
                            send_event(&proxy, UiEvent::UpdateStatus("App is up to date".into()));
                        }
                        Ok(())
                    })();
                    if let Err(error) = result {
                        log_diagnostic(&format!("automatic update failed: {error}"));
                        send_event(
                            &proxy,
                            UiEvent::UpdateStatus("Update failed; will retry later".into()),
                        );
                    }
                    next_check = Instant::now() + CHECK_INTERVAL;
                }
                let command = if automatic {
                    rx.recv_timeout(next_check.saturating_duration_since(Instant::now()))
                } else {
                    rx.recv().map_err(|_| RecvTimeoutError::Disconnected)
                };
                match command {
                    Ok(value) => {
                        automatic = value;
                        next_check = Instant::now();
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
        })?;
    Ok(tx)
}

// The backup is prepared on the worker. Replacement is a same-filesystem rename.
// If launching fails, restore the old binary before returning the error.
fn replace_and_launch(
    target: &Path,
    staging: &Path,
    launch: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    fs::rename(staging.join("new-executable.exe"), target)?;
    if let Err(error) = launch() {
        fs::rename(staging.join("previous-executable"), target).map_err(|rollback| {
            io::Error::other(format!(
                "restart failed: {error}; restoring old executable failed: {rollback}"
            ))
        })?;
        return Err(error);
    }
    Ok(())
}

impl PreparedUpdate {
    pub(super) fn install_and_restart(self) -> io::Result<()> {
        #[cfg(target_os = "linux")]
        {
            let result = replace_and_launch(&self.target, self.staging.path(), || {
                std::process::Command::new(&self.target).spawn().map(|_| ())
            });
            if result.is_err() && rollback_failed(self.staging.path()) {
                log_diagnostic(
                    "could not restore old executable; retaining update directory and backup",
                );
                let _ = self.staging.keep();
            }
            result
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            std::process::Command::new(self.staging.path().join("helper.exe"))
                .arg("--apply-update")
                .arg(&self.target)
                .arg(self.staging.path())
                .arg(std::process::id().to_string())
                .creation_flags(0x08000000)
                .spawn()?;
            // The helper now owns cleanup. It waits for this process to exit first.
            let _ = self.staging.keep();
            Ok(())
        }
    }
}

#[cfg(windows)]
fn wait_for_parent(pid: u32) -> io::Result<()> {
    use std::ffi::c_void;
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, milliseconds: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
    }
    // SYNCHRONIZE only. Never terminate the old process or request elevated rights.
    let handle = unsafe { OpenProcess(0x00100000, 0, pid) };
    if handle.is_null() {
        let error = io::Error::last_os_error();
        return if error.raw_os_error() == Some(87) {
            Ok(())
        } else {
            Err(error)
        };
    }
    let status = unsafe { WaitForSingleObject(handle, 60_000) };
    unsafe {
        CloseHandle(handle);
    }
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::other(
            "old app did not exit before update timeout",
        ))
    }
}

#[cfg(windows)]
fn valid_staging(target: &Path, staging: &Path) -> bool {
    target.is_absolute()
        && staging.is_absolute()
        && staging.parent() == target.parent()
        && staging
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with(STAGING_PREFIX))
}

fn rollback_failed(staging: &Path) -> bool {
    !staging.join("new-executable.exe").exists() && staging.join("previous-executable").exists()
}

pub(super) fn handle_helper_mode() -> Result<bool, Box<dyn Error>> {
    if std::env::args_os()
        .nth(1)
        .is_some_and(|a| a == "--update-health-check")
    {
        return Ok(true);
    }
    #[cfg(windows)]
    {
        let args = std::env::args_os().collect::<Vec<_>>();
        if args.get(1).is_some_and(|a| a == "--apply-update") {
            if args.len() != 5 {
                return Err("invalid update helper arguments".into());
            }
            let target = PathBuf::from(&args[2]);
            let staging = PathBuf::from(&args[3]);
            if !valid_staging(&target, &staging)
                || std::env::current_exe()? != staging.join("helper.exe")
            {
                return Err("invalid update staging directory".into());
            }
            let pid = args[4]
                .to_str()
                .ok_or("invalid parent process")?
                .parse::<u32>()?;
            wait_for_parent(pid).inspect_err(|error| {
                log_diagnostic(&format!(
                    "update helper could not wait for old app: {error}"
                ));
            })?;
            // Antivirus can briefly hold the old executable after process exit.
            let mut result = Err(io::Error::other("update was not installed"));
            for _ in 0..50 {
                if !enabled() {
                    break;
                }
                result = replace_and_launch(&target, &staging, || {
                    std::process::Command::new(&target)
                        .arg("--cleanup-update")
                        .arg(&staging)
                        .spawn()
                        .map(|_| ())
                });
                if result.is_ok() || !staging.join("new-executable.exe").exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(200));
            }
            if let Err(error) = result {
                log_diagnostic(&format!("update helper failed: {error}"));
                if rollback_failed(&staging) {
                    return Err(
                        "could not restore old executable; update directory and backup retained"
                            .into(),
                    );
                }
                std::process::Command::new(&target)
                    .arg("--cleanup-update")
                    .arg(&staging)
                    .arg("--update-retry-later")
                    .spawn()?;
            }
            return Ok(true);
        }
        if args.get(1).is_some_and(|a| a == "--cleanup-update") {
            let staging = args
                .get(2)
                .map(PathBuf::from)
                .ok_or("missing cleanup directory")?;
            if !valid_staging(&std::env::current_exe()?, &staging) {
                return Err("invalid cleanup directory".into());
            }
            thread::spawn(move || {
                // The helper must finish before Windows permits deleting helper.exe.
                for _ in 0..50 {
                    if fs::remove_dir_all(&staging).is_ok() {
                        return;
                    }
                    thread::sleep(Duration::from_millis(200));
                }
                log_diagnostic("could not remove completed update staging directory");
            });
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn asset(bytes: &[u8]) -> Asset {
        Asset {
            name: ASSET_NAME.into(),
            size: bytes.len() as u64,
            digest: Some(format!("sha256:{:x}", Sha256::digest(bytes))),
        }
    }

    fn release(tag: &str) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease: false,
            assets: vec![asset(b"update")],
        }
    }

    #[test]
    fn only_newer_published_builds_for_this_platform_are_updates() {
        assert_eq!(newer_build(&release("build-12"), 11), Some(12));
        for tag in [
            "build-12",
            "build-11",
            "v0.1.0",
            "build-bad",
            "build-18446744073709551616",
        ] {
            assert_eq!(newer_build(&release(tag), 12), None);
        }
        let mut candidate = release("build-13");
        candidate.draft = true;
        assert_eq!(newer_build(&candidate, 0), None);
        candidate.draft = false;
        candidate.prerelease = true;
        assert_eq!(newer_build(&candidate, 0), None);
        candidate.prerelease = false;
        candidate.assets.clear();
        assert_eq!(newer_build(&candidate, 0), None);
    }

    #[test]
    fn downloads_require_matching_size_and_digest() {
        let expected = asset(b"new executable");
        let mut output = Vec::new();
        verify_download(&mut &b"new executable"[..], &mut output, &expected, || true).unwrap();
        assert_eq!(output, b"new executable");
        for bytes in [
            &b"new executablX"[..],
            &b"short"[..],
            &b"new executable extra"[..],
        ] {
            assert!(verify_download(&mut &bytes[..], &mut Vec::new(), &expected, || true).is_err());
        }
        for digest in [
            None,
            Some("sha256:bad".into()),
            Some(format!("sha256:{}", "x".repeat(64))),
        ] {
            let mut invalid = asset(b"new executable");
            invalid.digest = digest;
            assert!(
                verify_download(
                    &mut &b"new executable"[..],
                    &mut Vec::new(),
                    &invalid,
                    || true
                )
                .is_err()
            );
        }
        let mut oversized = asset(b"new executable");
        oversized.size = MAX_DOWNLOAD_SIZE + 1;
        assert!(verify_download(&mut io::empty(), &mut Vec::new(), &oversized, || true).is_err());
    }

    #[test]
    fn disabling_updates_cancels_a_download() {
        assert!(
            verify_download(
                &mut &b"update"[..],
                &mut Vec::new(),
                &asset(b"update"),
                || false
            )
            .is_err()
        );
    }

    fn staged_files() -> (TempDir, PathBuf, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("app");
        let staging = root.path().join("staging");
        fs::create_dir(&staging).unwrap();
        fs::write(&target, b"old").unwrap();
        fs::write(staging.join("previous-executable"), b"old").unwrap();
        fs::write(staging.join("new-executable.exe"), b"new").unwrap();
        (root, target, staging)
    }

    #[test]
    fn replacement_is_installed_before_launch() {
        let (_root, target, staging) = staged_files();
        replace_and_launch(&target, &staging, || {
            assert_eq!(fs::read(&target).unwrap(), b"new");
            Ok(())
        })
        .unwrap();
        assert_eq!(fs::read(target).unwrap(), b"new");
    }

    #[test]
    fn launch_failure_restores_previous_executable() {
        let (_root, target, staging) = staged_files();
        assert!(
            replace_and_launch(&target, &staging, || Err(io::Error::other("cannot launch")))
                .is_err()
        );
        assert_eq!(fs::read(target).unwrap(), b"old");
    }

    #[test]
    fn replacement_failure_keeps_original_and_does_not_launch() {
        let (_root, target, staging) = staged_files();
        fs::remove_file(staging.join("new-executable.exe")).unwrap();
        assert!(replace_and_launch(&target, &staging, || panic!("must not launch")).is_err());
        assert_eq!(fs::read(target).unwrap(), b"old");
    }

    #[test]
    fn failed_rollback_retains_the_previous_executable() {
        let (_root, target, staging) = staged_files();
        assert!(
            replace_and_launch(&target, &staging, || {
                fs::remove_file(&target)?;
                fs::create_dir(&target)?;
                Err(io::Error::other("cannot launch"))
            })
            .is_err()
        );
        assert!(rollback_failed(&staging));
        assert_eq!(
            fs::read(staging.join("previous-executable")).unwrap(),
            b"old"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_install_restarts_from_the_original_path() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::Builder::new()
            .prefix("pbm test ")
            .tempdir()
            .unwrap();
        let target = root.path().join("app with spaces");
        let staging = tempfile::Builder::new()
            .prefix(STAGING_PREFIX)
            .tempdir_in(root.path())
            .unwrap();
        fs::write(&target, b"old").unwrap();
        fs::copy(&target, staging.path().join("previous-executable")).unwrap();
        let downloaded = staging.path().join("new-executable.exe");
        fs::write(
            &downloaded,
            b"#!/bin/sh\nprintf restarted > \"$0.started\"\n",
        )
        .unwrap();
        fs::set_permissions(&downloaded, fs::Permissions::from_mode(0o755)).unwrap();
        PreparedUpdate {
            staging,
            target: target.clone(),
        }
        .install_and_restart()
        .unwrap();
        let marker = target.with_file_name("app with spaces.started");
        let deadline = Instant::now() + Duration::from_secs(2);
        while !marker.exists() && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(fs::read(marker).unwrap(), b"restarted");
    }

    #[cfg(windows)]
    #[test]
    fn helper_cleanup_is_limited_to_sibling_staging_directories() {
        let target = Path::new(r"C:\Apps\monitor.exe");
        assert!(valid_staging(target, Path::new(r"C:\Apps\.pbm-update-abc")));
        assert!(!valid_staging(
            target,
            Path::new(r"C:\Other\.pbm-update-abc")
        ));
        assert!(!valid_staging(target, Path::new(r"C:\Apps\unrelated")));
        assert!(!valid_staging(target, Path::new(r".pbm-update-abc")));
    }
}
