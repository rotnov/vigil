//! Estimates time-to-empty from the battery percentage drop `vigil` itself
//! observes over time.
//!
//! Deliberately does not attribute power draw to individual processes —
//! that needs `powermetrics --show-process-energy`, which requires sudo,
//! and the project decided against adding a privilege escalation for that.
//! This only extrapolates a linear drain rate from repeated snapshots.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

const MAX_SAMPLES: usize = 20;
const MIN_SAMPLES_FOR_ETA: usize = 3;

pub struct BatteryTrend {
    samples: VecDeque<(Instant, u8)>,
}

impl BatteryTrend {
    pub fn new() -> Self {
        Self {
            samples: VecDeque::with_capacity(MAX_SAMPLES),
        }
    }

    /// Feed one observation. Clears the trend whenever the battery isn't
    /// actively discharging (plugged in, or state unknown) — a rate
    /// computed across a charge event would be meaningless, and pmset's
    /// own `charging` flag flipping is the clearest signal that happened.
    pub fn record(&mut self, charging: Option<bool>, percentage: Option<u8>, now: Instant) {
        let (Some(false), Some(pct)) = (charging, percentage) else {
            self.samples.clear();
            return;
        };
        if self.samples.len() == MAX_SAMPLES {
            self.samples.pop_front();
        }
        self.samples.push_back((now, pct));
    }

    /// Extrapolated time until 0%, based on the drain rate across the
    /// current sample window. `None` if there isn't enough discharging
    /// history yet, or the percentage hasn't actually dropped (flat/rising
    /// readings can't yield a rate).
    pub fn eta(&self) -> Option<Duration> {
        if self.samples.len() < MIN_SAMPLES_FOR_ETA {
            return None;
        }
        let (t0, p0) = *self.samples.front().unwrap();
        let (t1, p1) = *self.samples.back().unwrap();
        if p1 >= p0 {
            return None;
        }

        let elapsed_secs = t1.duration_since(t0).as_secs_f64();
        let dropped_pct = (p0 - p1) as f64;
        if elapsed_secs <= 0.0 {
            return None;
        }

        let secs_per_percent = elapsed_secs / dropped_pct;
        Some(Duration::from_secs_f64(secs_per_percent * p1 as f64))
    }
}

/// Formats a duration as e.g. "1h42m" or "45m", for notifications/UI.
pub fn format_eta(d: Duration) -> String {
    let total_min = d.as_secs() / 60;
    let h = total_min / 60;
    let m = total_min % 60;
    if h > 0 {
        format!("{h}h{m:02}m")
    } else {
        format!("{m}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_eta_without_enough_samples() {
        let mut t = BatteryTrend::new();
        let now = Instant::now();
        t.record(Some(false), Some(90), now);
        t.record(Some(false), Some(89), now + Duration::from_secs(60));
        assert!(t.eta().is_none(), "2 samples is below MIN_SAMPLES_FOR_ETA");
    }

    #[test]
    fn no_eta_while_charging() {
        let mut t = BatteryTrend::new();
        let now = Instant::now();
        for i in 0..5 {
            t.record(Some(true), Some(50 + i), now + Duration::from_secs(i as u64 * 60));
        }
        assert!(t.eta().is_none());
    }

    #[test]
    fn no_eta_when_charging_state_unknown() {
        let mut t = BatteryTrend::new();
        let now = Instant::now();
        for i in 0..5 {
            t.record(None, Some(50), now + Duration::from_secs(i * 60));
        }
        assert!(t.eta().is_none());
    }

    #[test]
    fn no_eta_when_percentage_flat_or_rising() {
        let mut t = BatteryTrend::new();
        let now = Instant::now();
        t.record(Some(false), Some(80), now);
        t.record(Some(false), Some(80), now + Duration::from_secs(300));
        t.record(Some(false), Some(81), now + Duration::from_secs(600));
        assert!(t.eta().is_none(), "percentage went up, not down");
    }

    #[test]
    fn eta_is_reasonable_for_steady_drain() {
        let mut t = BatteryTrend::new();
        let now = Instant::now();
        // 100% -> 90% over 10 minutes => 60s per percent => 90% left = 5400s = 90min
        t.record(Some(false), Some(100), now);
        t.record(Some(false), Some(95), now + Duration::from_secs(300));
        t.record(Some(false), Some(90), now + Duration::from_secs(600));

        let eta = t.eta().expect("should have an ETA with 3 discharging samples");
        let expected = Duration::from_secs(90 * 60);
        let diff = eta.as_secs().abs_diff(expected.as_secs());
        assert!(diff < 5, "expected ~{expected:?}, got {eta:?}");
    }

    #[test]
    fn resets_when_charging_resumes() {
        let mut t = BatteryTrend::new();
        let now = Instant::now();
        t.record(Some(false), Some(100), now);
        t.record(Some(false), Some(95), now + Duration::from_secs(300));
        t.record(Some(false), Some(90), now + Duration::from_secs(600));
        assert!(t.eta().is_some());

        t.record(Some(true), Some(91), now + Duration::from_secs(660));
        assert!(t.eta().is_none(), "a charging sample must clear the discharge trend");

        // and it takes MIN_SAMPLES_FOR_ETA fresh discharging samples to recover
        t.record(Some(false), Some(90), now + Duration::from_secs(720));
        assert!(t.eta().is_none());
    }

    #[test]
    fn format_eta_hours_and_minutes() {
        assert_eq!(format_eta(Duration::from_secs(90 * 60)), "1h30m");
        assert_eq!(format_eta(Duration::from_secs(45 * 60)), "45m");
        assert_eq!(format_eta(Duration::from_secs(0)), "0m");
    }
}
