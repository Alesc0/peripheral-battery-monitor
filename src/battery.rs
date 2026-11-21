use std::collections::VecDeque;

// Source: https://www.scyrox.net/assets/index-DUfu8aky.js, yo / Fv.
const CURVE_MV: [u16; 21] = [
    3050, 3420, 3480, 3540, 3600, 3660, 3720, 3760, 3800, 3840, 3880, 3920, 3940, 3960, 3980, 4000,
    4020, 4040, 4060, 4080, 4110,
];

pub(super) fn voltage(response: &[u8]) -> Option<u16> {
    let mv = u16::from_be_bytes([*response.get(7)?, *response.get(8)?]);
    (2800..=4500).contains(&mv).then_some(mv)
}

fn percentage(mv: u16) -> u8 {
    if mv <= CURVE_MV[0] {
        return 0;
    }
    if mv >= CURVE_MV[20] {
        return 100;
    }
    let upper = CURVE_MV.partition_point(|point| *point <= mv);
    let span = u32::from(CURVE_MV[upper] - CURVE_MV[upper - 1]);
    let progress = u32::from(mv - CURVE_MV[upper - 1]);
    ((upper - 1) * 5) as u8 + ((progress * 5 + span / 2) / span) as u8
}

#[derive(Clone, Debug)]
pub(super) struct History {
    pub at: u64,
    pub level: u8,
    filtered: f64,
    charging: bool,
    step_at: u64,
    settle_until: u64,
    full: bool,
}

impl History {
    pub fn encode(&self) -> String {
        format!(
            "1 {} {} {:.3} {} {} {} {}",
            self.at,
            self.level,
            self.filtered,
            u8::from(self.charging),
            self.step_at,
            self.settle_until,
            u8::from(self.full)
        )
    }

    pub fn decode(text: &str, now: u64) -> Option<Self> {
        let fields: Vec<_> = text.split_whitespace().collect();
        if fields.len() != 8 || fields[0] != "1" {
            return None;
        }
        let boolean = |value| match value {
            "0" => Some(false),
            "1" => Some(true),
            _ => None,
        };
        let state = Self {
            at: fields[1].parse().ok()?,
            level: fields[2].parse().ok()?,
            filtered: fields[3].parse().ok()?,
            charging: boolean(fields[4])?,
            step_at: fields[5].parse().ok()?,
            settle_until: fields[6].parse().ok()?,
            full: boolean(fields[7])?,
        };
        (state.at <= now
            && state.level <= 100
            && state.filtered.is_finite()
            && (state.filtered == 0.0 || (2800.0..=4500.0).contains(&state.filtered))
            && state.step_at <= state.at
            && state.settle_until <= state.at.saturating_add(120))
        .then_some(state)
    }
}

#[derive(Clone, Copy, Debug)]
pub(super) struct Sample {
    pub at: u64,
    pub raw: u8,
    pub mv: Option<u16>,
    pub filtered: Option<u16>,
    pub target: u8,
    pub level: u8,
    pub charging: bool,
    pub full: bool,
}

#[derive(Default)]
pub(super) struct Estimator {
    pub history: Option<History>,
    samples: VecDeque<(u64, u16)>,
    full_count: u8,
    full_started: u64,
    full_at: u64,
}

impl Estimator {
    pub fn new(history: Option<History>) -> Self {
        Self {
            history,
            ..Self::default()
        }
    }

    pub fn seed(&mut self, at: u64, level: u8, charging: bool) {
        self.history = Some(History {
            at,
            level: level.min(100),
            filtered: 0.0,
            charging,
            step_at: at,
            settle_until: 0,
            full: false,
        });
    }

    pub fn observe(&mut self, at: u64, raw: u8, mv: Option<u16>, charging: bool) -> Sample {
        let raw = raw.min(100);
        let previous = self.history.as_ref();
        let elapsed = previous.map_or(0, |state| at.saturating_sub(state.at));
        let transition = previous.is_some_and(|state| state.charging != charging);
        let reset = elapsed > 120 || transition || previous.is_some_and(|state| at < state.at);
        if reset {
            self.samples.clear();
        }
        if reset || elapsed > 15 {
            self.full_count = 0;
        }
        while self
            .samples
            .front()
            .is_some_and(|(time, _)| at.saturating_sub(*time) > 60)
        {
            self.samples.pop_front();
        }
        if let Some(mv) = mv
            && self.samples.back().is_none_or(|(time, _)| *time != at)
        {
            self.samples.push_back((at, mv));
        }
        // Median rejects isolated spikes; a 30-second EMA smooths normal variation.
        let filtered = mv.map(|_| {
            let mut values: Vec<_> = self.samples.iter().map(|(_, mv)| *mv).collect();
            values.sort_unstable();
            let median = f64::from(values[values.len() / 2]);
            match previous.filter(|state| !reset && state.filtered > 0.0) {
                Some(state) => {
                    state.filtered
                        + (median - state.filtered) * (1.0 - (-(elapsed as f64) / 30.0).exp())
                }
                None => median,
            }
        });
        // Eight full reports must span at least 35 seconds, without polling gaps.
        if charging && raw == 100 {
            if self.full_count == 0 {
                self.full_started = at;
            }
            if self.full_count == 0 || at.saturating_sub(self.full_at) >= 4 {
                self.full_count = self.full_count.saturating_add(1);
                self.full_at = at;
            }
        } else {
            self.full_count = 0;
        }
        let confirmed =
            charging && self.full_count >= 8 && at.saturating_sub(self.full_started) >= 35;
        let mut full =
            confirmed || previous.is_some_and(|state| state.full && !transition && !reset);
        if charging && raw < 100 {
            full = false;
        }
        if transition && !charging {
            full = previous.is_some_and(|state| state.full);
        }
        let mut target = filtered.map_or(raw, |value| percentage(value.round() as u16));
        if !full {
            target = target.min(99);
        }
        if confirmed {
            target = 100;
        }
        let settle_until = if transition && !charging {
            at.saturating_add(120)
        } else {
            previous.map_or(0, |state| state.settle_until)
        };
        let mut level = previous.map_or(target, |state| state.level);
        let mut step_at = previous.map_or(at, |state| state.step_at);
        if confirmed {
            level = 100;
            step_at = at;
        } else if previous.is_none() || elapsed > 1800 {
            level = target;
            step_at = at;
        } else if reset {
            step_at = at;
        } else if at >= settle_until && at.saturating_sub(step_at) >= 30 {
            // One point per step, with no catch-up jumps after sleep or disconnects.
            if charging && level < target {
                level += 1;
            }
            if !charging && level > target {
                level -= 1;
            }
            step_at = at;
        }
        // Coarse receiver warnings override smoothing; never hide an empty battery.
        if !charging && raw <= 15 {
            level = level.min(raw);
        }
        if !charging && level < 100 {
            full = false;
        }
        self.history = Some(History {
            at,
            level,
            filtered: filtered.unwrap_or(0.0),
            charging,
            step_at,
            settle_until,
            full,
        });
        Sample {
            at,
            raw,
            mv,
            filtered: filtered.map(|value| value.round() as u16),
            target,
            level,
            charging,
            full: confirmed,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interpolation_and_invalid_voltage() {
        assert_eq!(percentage(4072), 93);
        for (i, mv) in CURVE_MV.iter().enumerate() {
            assert_eq!(percentage(*mv), (i * 5) as u8);
        }
        let mut previous = 0;
        for mv in 2800..=4500 {
            let level = percentage(mv);
            assert!((previous..=100).contains(&level));
            previous = level;
        }
        assert_eq!(voltage(&[0; 8]), None);
        for mv in [0u16, 2799, 4501, u16::MAX] {
            let mut response = [0; 9];
            response[7..9].copy_from_slice(&mv.to_be_bytes());
            assert_eq!(voltage(&response), None);
        }
    }
    #[test]
    fn spike_rejection_and_discharge_rebound() {
        let mut e = Estimator::default();
        assert_eq!(e.observe(100, 80, Some(4020), false).level, 80);
        e.observe(105, 80, Some(4020), false);
        assert_eq!(e.observe(110, 80, Some(3000), false).target, 80);
        assert_eq!(e.observe(140, 85, Some(4100), false).level, 80);
    }
    #[test]
    fn persists_history_and_ignores_corruption() {
        let mut e = Estimator::default();
        e.observe(100, 80, Some(4020), false);
        let text = e.history.unwrap().encode();
        let mut restored = Estimator::new(History::decode(&text, 110));
        assert_eq!(restored.observe(110, 95, Some(4109), false).level, 80);
        for bad in [
            "",
            "2 100 80 4020 0 100 0 0",
            "1 100 101 4020 0 100 0 0",
            "1 100 80 NaN 0 100 0 0",
            "1 100 80 4020 2 100 0 0",
        ] {
            assert!(History::decode(bad, 110).is_none());
        }
        assert!(History::decode(&text, 99).is_none());
    }
    #[test]
    fn full_requires_continuous_reports_and_time() {
        let mut e = Estimator::default();
        for at in 100..108 {
            assert!(e.observe(at, 100, Some(4200), true).level < 100);
        }
        for at in (110..=140).step_by(5) {
            e.observe(at, 100, Some(4200), true);
        }
        assert_eq!(e.history.as_ref().unwrap().level, 100);
        let mut e = Estimator::default();
        e.observe(100, 100, Some(4200), true);
        assert!(e.observe(300, 100, Some(4200), true).level < 100);
    }
    #[test]
    fn unplug_settles_and_missing_voltage_falls_back() {
        let mut e = Estimator::default();
        for at in (100..=135).step_by(5) {
            e.observe(at, 100, Some(4200), true);
        }
        assert_eq!(e.observe(140, 60, Some(3880), false).level, 100);
        assert_eq!(e.observe(250, 60, Some(3880), false).level, 100);
        assert_eq!(e.observe(260, 60, Some(3880), false).level, 99);
        assert_eq!(e.observe(265, 5, None, false).level, 5);
        assert_eq!(Estimator::default().observe(100, 65, None, false).level, 65);
    }
    #[test]
    fn sleep_does_not_count_as_continuous_discharge() {
        let mut e = Estimator::default();
        e.observe(100, 80, Some(4020), false);
        assert_eq!(e.observe(300, 40, Some(3800), false).level, 80);
        assert_eq!(e.observe(330, 40, Some(3800), false).level, 79);
        assert_eq!(e.observe(3000, 40, Some(3800), false).level, 40);
    }
}
