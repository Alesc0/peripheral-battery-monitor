use super::{
    battery::{Estimator, History, Sample},
    *,
};
use std::{
    io::{self, Read},
    path::Path,
};

const MAX_BYTES: u64 = 2 * 1024 * 1024;
const HEADER: &str = "epoch,raw_percent,voltage_mv,filtered_mv,target_percent,display_percent,charging,confirmed_full\n";

#[derive(Default)]
pub(super) struct Tracking {
    devices: HashMap<String, Tracker>,
}
struct Tracker {
    estimator: Estimator,
    logged_at: Option<u64>,
    previous_mode: Option<(bool, bool)>,
}

impl Tracking {
    pub fn observe(
        &mut self,
        id: &str,
        name: &str,
        raw: u8,
        mv: Option<u16>,
        charging: bool,
    ) -> u8 {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let tracker = self.devices.entry(id.to_owned()).or_insert_with(|| {
            let history = load_history(id).and_then(|text| History::decode(&text, now));
            let mut estimator = Estimator::new(history);
            if estimator.history.is_none()
                && let Some(cached) = load_cached_battery(id, name)
                && cached.charging == charging
                && cached.level.abs_diff(raw) <= 15
            {
                estimator.seed(now, cached.level, cached.charging);
            }
            Tracker {
                estimator,
                logged_at: None,
                previous_mode: None,
            }
        });
        let sample = tracker.estimator.observe(now, raw, mv, charging);
        let mode = (charging, sample.full);
        if tracker
            .logged_at
            .is_none_or(|last| now.saturating_sub(last) >= 30)
            || tracker.previous_mode != Some(mode)
        {
            // Failures never prevent the UI from receiving a battery reading.
            if let Err(error) =
                save_history(id, &tracker.estimator.history.as_ref().unwrap().encode())
            {
                log_diagnostic(&format!("could not save estimator history: {error}"));
            }
            if let Err(error) = measurement_path(id).and_then(|path| append_sample(&path, sample)) {
                log_diagnostic(&format!("could not save battery measurements: {error}"));
            }
            tracker.logged_at = Some(now);
            tracker.previous_mode = Some(mode);
        }
        sample.level
    }
}

fn directory() -> io::Result<PathBuf> {
    diagnostic_log_path()
        .and_then(|path| path.parent().map(Path::to_path_buf))
        .map(|path| path.join("battery-measurements"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "battery state directory unavailable",
            )
        })
}
fn measurement_path(id: &str) -> io::Result<PathBuf> {
    Ok(directory()?.join(format!("{}.csv", alarm_registry_value_name(id))))
}

#[cfg(target_os = "linux")]
fn load_history(id: &str) -> Option<String> {
    fs::read_to_string(
        directory()
            .ok()?
            .join(format!("{}.history", alarm_registry_value_name(id))),
    )
    .ok()
}
#[cfg(target_os = "linux")]
fn save_history(id: &str, text: &str) -> io::Result<()> {
    linux::write_file(
        &directory()?.join(format!("{}.history", alarm_registry_value_name(id))),
        text,
    )
}
#[cfg(windows)]
fn load_history(id: &str) -> Option<String> {
    RegKey::predef(HKEY_CURRENT_USER)
        .open_subkey(battery_cache_key(id))
        .ok()?
        .get_value("EstimatorHistory")
        .ok()
}
#[cfg(windows)]
fn save_history(id: &str, text: &str) -> io::Result<()> {
    let (key, _) = RegKey::predef(HKEY_CURRENT_USER).create_subkey(battery_cache_key(id))?;
    key.set_value("EstimatorHistory", &text)
}

fn append_sample(path: &Path, sample: Sample) -> io::Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    if fs::metadata(path).is_ok_and(|meta| meta.len() >= MAX_BYTES) {
        let previous = path.with_extension("csv.1");
        match fs::remove_file(&previous) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
        fs::rename(path, previous)?;
    }
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    if file.metadata()?.len() == 0 {
        file.write_all(HEADER.as_bytes())?;
    }
    writeln!(
        file,
        "{},{},{},{},{},{},{},{}",
        sample.at,
        sample.raw,
        sample.mv.map(|v| v.to_string()).unwrap_or_default(),
        sample.filtered.map(|v| v.to_string()).unwrap_or_default(),
        sample.target,
        sample.level,
        u8::from(sample.charging),
        u8::from(sample.full)
    )
}

pub(super) fn report() -> io::Result<()> {
    let dir = directory()?;
    println!("Battery calibration measurements: {}", dir.display());
    let entries = match fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            println!("No measurements yet. Run the tray app during normal use.");
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut paths = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "csv"))
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        let mut rows = Vec::new();
        for source in [path.with_extension("csv.1"), path.clone()] {
            if let Ok(file) = fs::File::open(source) {
                let mut text = String::new();
                file.take(MAX_BYTES + 1024).read_to_string(&mut text)?;
                rows.extend(text.lines().filter_map(parse_row));
            }
        }
        println!("\n{}", path.file_stem().unwrap().to_string_lossy());
        println!("{} recorded live samples", rows.len());
        let summary = summarize(&rows);
        println!(
            "{} full-to-low cycles; {} with continuous coverage",
            summary.cycles, summary.continuous
        );
        if summary.continuous == 0 {
            println!(
                "Need a confirmed full charge followed by normal use down to <=5% receiver level."
            );
            println!("Gaps over 120 seconds exclude a cycle from curve comparisons.");
        } else {
            println!(
                "Reference: remaining wall-clock runtime to <=5%, not measured charge capacity."
            );
            println!(
                "Mean absolute difference: curve {:.1} points; tray {:.1} points",
                summary.curve_error / summary.compared as f64,
                summary.display_error / summary.compared as f64
            );
            println!(
                "Use comparable polling rates and workloads across several cycles before adjusting the curve."
            );
        }
    }
    println!("\nMeasurements stay local. No curve changes are applied automatically.");
    Ok(())
}

fn parse_row(line: &str) -> Option<Sample> {
    let f: Vec<_> = line.split(',').collect();
    if f.len() != 8 {
        return None;
    }
    let boolean = |s| match s {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    };
    let raw = f[1].parse::<u8>().ok()?;
    let target = f[4].parse::<u8>().ok()?;
    let level = f[5].parse::<u8>().ok()?;
    if raw > 100 || target > 100 || level > 100 {
        return None;
    }
    let optional_mv = |s: &str| {
        if s.is_empty() {
            Some(None)
        } else {
            s.parse::<u16>()
                .ok()
                .filter(|v| (2800..=4500).contains(v))
                .map(Some)
        }
    };
    Some(Sample {
        at: f[0].parse().ok()?,
        raw,
        mv: optional_mv(f[2])?,
        filtered: optional_mv(f[3])?,
        target,
        level,
        charging: boolean(f[6])?,
        full: boolean(f[7])?,
    })
}

#[derive(Default)]
struct Summary {
    cycles: usize,
    continuous: usize,
    compared: usize,
    curve_error: f64,
    display_error: f64,
}
fn summarize(rows: &[Sample]) -> Summary {
    let mut result = Summary::default();
    let mut armed = false;
    let mut cycle: Vec<Sample> = Vec::new();
    let mut gap = false;
    let mut previous = None;
    for &sample in rows {
        if previous.is_some_and(|at| sample.at <= at || sample.at.saturating_sub(at) > 120) {
            if !cycle.is_empty() {
                gap = true;
            } else {
                armed = false;
            }
        }
        previous = Some(sample.at);
        if sample.charging {
            cycle.clear();
            gap = false;
            armed = sample.full;
            continue;
        }
        if !armed {
            continue;
        }
        cycle.push(sample);
        if sample.raw <= 5 {
            result.cycles += 1;
            let start = cycle[0].at;
            let duration = sample.at.saturating_sub(start);
            if !gap && duration >= 600 && cycle.len() >= 20 && cycle.iter().all(|s| s.mv.is_some())
            {
                result.continuous += 1;
                for s in &cycle {
                    let remaining = 100.0 * (sample.at - s.at) as f64 / duration as f64;
                    result.curve_error += (f64::from(s.target) - remaining).abs();
                    result.display_error += (f64::from(s.level) - remaining).abs();
                    result.compared += 1;
                }
            }
            cycle.clear();
            armed = false;
            gap = false;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample(at: u64, raw: u8, charging: bool, full: bool) -> Sample {
        Sample {
            at,
            raw,
            mv: Some(4000),
            filtered: Some(4000),
            target: raw,
            level: raw,
            charging,
            full,
        }
    }
    #[test]
    fn needs_full_to_low_and_rejects_sleep_gaps() {
        let mut rows = vec![sample(100, 100, true, true)];
        for i in 0..=20 {
            rows.push(sample(130 + i * 35, (100 - i * 5) as u8, false, false));
        }
        assert_eq!(summarize(&rows).continuous, 1);
        assert_eq!(summarize(&rows[1..]).cycles, 0);
        for s in &mut rows[10..] {
            s.at += 1000;
        }
        let summary = summarize(&rows);
        assert_eq!(summary.cycles, 1);
        assert_eq!(summary.continuous, 0);
    }
    #[test]
    fn rotation_and_partial_rows_are_safe() {
        let dir = std::env::temp_dir().join(format!("pbm-measurement-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.csv");
        let file = fs::File::create(&path).unwrap();
        file.set_len(MAX_BYTES).unwrap();
        append_sample(&path, sample(100, 95, false, false)).unwrap();
        assert!(path.with_extension("csv.1").exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(HEADER));
        assert_eq!(parse_row(text.lines().nth(1).unwrap()).unwrap().raw, 95);
        for invalid in [
            "",
            "epoch,raw",
            "100,95,4000",
            "100,101,4000,4000,90,90,0,0",
        ] {
            assert!(parse_row(invalid).is_none());
        }
        fs::remove_dir_all(dir).unwrap();
    }
}
