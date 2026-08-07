//! Menu bar health indicator.
//!
//! Reads the small status file `vigil watch` writes every tick and shows a
//! colored (or, when healthy, mostly-transparent) macOS menu bar icon —
//! deliberately *not* a second sampling/evaluation loop of its own. See
//! docs/decisions/0002-menu-bar-health-indicator.md for why.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// `~/.vigil/status.json` — same home-relative convention as
/// `incidents::default_dir`, for the same reason (vigil runs from anywhere).
pub fn default_status_file() -> PathBuf {
    // Coverage exemption (see AGENTS.md's testing section): same reasoning
    // as `incidents::default_dir`'s identical fallback -- `$HOME` is always
    // set for a real `cargo test` run, and mutating it would race every
    // other test in this same-process suite that also reads it.
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."));
    home.join(".vigil").join("status.json")
}

#[derive(Serialize, Deserialize)]
pub(crate) struct StatusFile {
    updated_unix: u64,
    open_count: usize,
}

/// Pure: builds the JSON body `vigil watch` writes. Kept separate from the
/// actual file write below so it's testable without touching the filesystem.
fn build_status_json(open_count: usize, now_unix: u64) -> String {
    serde_json::to_string(&StatusFile { updated_unix: now_unix, open_count }).unwrap()
}

/// Called from `vigil watch`'s tick loop. A write failure is logged, not
/// fatal — a missing/stale status file just reads as `HealthLevel::Unknown`
/// in the menu bar, which is the honest outcome for "watch isn't updating
/// this" anyway.
pub fn write_status(path: &str, open_count: usize) {
    let now_unix = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    if let Err(e) = std::fs::write(path, build_status_json(open_count, now_unix)) {
        eprintln!("[vigil] failed to write status file {path}: {e}");
    }
}

pub(crate) fn read_status(path: &Path) -> Option<StatusFile> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HealthLevel {
    Ok,
    Warning,
    Critical,
    /// No status file, or one old enough that `vigil watch` looks dead —
    /// shown distinctly rather than defaulting to `Ok`, since "no data" and
    /// "confirmed healthy" are different facts.
    Unknown,
}

/// Pure classification — no I/O. `status` is already-parsed file content
/// (`None` if the file is missing/unparseable).
pub(crate) fn classify_health(status: Option<&StatusFile>, now_unix: u64, stale_after_secs: u64) -> HealthLevel {
    let Some(status) = status else { return HealthLevel::Unknown };
    if now_unix.saturating_sub(status.updated_unix) > stale_after_secs {
        return HealthLevel::Unknown;
    }
    match status.open_count {
        0 => HealthLevel::Ok,
        1 => HealthLevel::Warning,
        _ => HealthLevel::Critical,
    }
}

const ICON_SIZE: u32 = 22;

/// Two overlapping circles, centers offset vertically, produce a vesica
/// piscis (lens) shape pointed at its left/right corners -- a horizontal
/// almond, i.e. an eye outline. Tuned by hand against actual rendered PNG
/// previews at `ICON_SIZE` (see the design commit) since there's no way to
/// preview a rendered icon directly in this environment — a throwaway test
/// dumped the RGBA buffers to a file, rendered with Pillow, viewed as an
/// image. A first pass without anti-aliasing read as an angular hexagon
/// rather than a smooth almond at this resolution, and a same-hue pupil
/// (just a higher-alpha version of the eye's own color) was too subtle to
/// register as a pupil at all — both addressed below.
const EYE_FOCAL_OFFSET: f32 = 6.5;
const EYE_RADIUS: f32 = 9.5;
const PUPIL_RADIUS: f32 = 2.6;
/// How far the pupil's RGB is mixed toward black, relative to the eye's own
/// color — real contrast instead of an alpha bump, which barely reads at
/// `ICON_SIZE`.
const PUPIL_DARKEN: f32 = 0.65;

/// Pure — signed-distance-ish coverage (0.0 outside, 1.0 solidly inside,
/// a ~1px soft transition between) for the eye outline at `(x, y)` on an
/// icon of the given `size`. Anti-aliasing this instead of a hard boolean
/// test is what turns the two-circle intersection into a smooth almond
/// instead of a blocky hexagon at 22px.
fn eye_coverage(x: f32, y: f32, size: f32) -> f32 {
    let center = size / 2.0;
    let d1 = ((x - center).powi(2) + (y - (center - EYE_FOCAL_OFFSET)).powi(2)).sqrt();
    let d2 = ((x - center).powi(2) + (y - (center + EYE_FOCAL_OFFSET)).powi(2)).sqrt();
    (EYE_RADIUS - d1).min(EYE_RADIUS - d2).clamp(-0.5, 0.5) + 0.5
}

/// Pure — coverage (see `eye_coverage`) for the pupil, the small dot at the
/// eye's center.
fn pupil_coverage(x: f32, y: f32, size: f32) -> f32 {
    let center = size / 2.0;
    let d = ((x - center).powi(2) + (y - center).powi(2)).sqrt();
    (PUPIL_RADIUS - d).clamp(-0.5, 0.5) + 0.5
}

/// Whether the pixel center `(x, y)` falls at least partly inside the eye
/// outline. A boolean view of `eye_coverage`, only for tests that want
/// in/out rather than the anti-aliasing weight `icon_rgba` itself uses.
#[cfg(test)]
fn is_inside_eye(x: f32, y: f32, size: f32) -> bool {
    eye_coverage(x, y, size) > 0.0
}

/// Whether `(x, y)` falls at least partly inside the pupil — test-only,
/// see `is_inside_eye`.
#[cfg(test)]
fn is_inside_pupil(x: f32, y: f32, size: f32) -> bool {
    pupil_coverage(x, y, size) > 0.0
}

fn lerp_u8(from: u8, to: u8, t: f32) -> u8 {
    (from as f32 + (to as f32 - from as f32) * t.clamp(0.0, 1.0)).round() as u8
}

/// vigil's tray icon: a small eye — watchfulness is literally what "vigil"
/// means — drawn procedurally rather than loaded from a bundled asset, see
/// the ADR. `Ok` is a faint outline rather than fully invisible pixels, so
/// the tray item stays locatable while still reading as "nothing to see
/// here". The pupil is mixed toward black (not just a higher alpha) for
/// real contrast against the eye's health-color body at every level,
/// including `Ok` — a small darker dot inside an otherwise near-invisible
/// shape still reads as unobtrusive, not as a second thing demanding
/// attention.
pub(crate) fn icon_rgba(level: HealthLevel) -> (Vec<u8>, u32, u32) {
    let (r, g, b, a): (u8, u8, u8, u8) = match level {
        HealthLevel::Ok => (255, 255, 255, 40),
        HealthLevel::Warning => (255, 190, 20, 255),
        HealthLevel::Critical => (230, 50, 50, 255),
        HealthLevel::Unknown => (140, 140, 140, 200),
    };
    let (pr, pg, pb) = (lerp_u8(r, 0, PUPIL_DARKEN), lerp_u8(g, 0, PUPIL_DARKEN), lerp_u8(b, 0, PUPIL_DARKEN));
    let size = ICON_SIZE;
    let size_f = size as f32;
    let mut buf = vec![0u8; (size * size * 4) as usize];
    for y in 0..size {
        for x in 0..size {
            let cx = x as f32 + 0.5;
            let cy = y as f32 + 0.5;
            let eye_cov = eye_coverage(cx, cy, size_f);
            if eye_cov <= 0.0 {
                continue;
            }
            let pupil_t = pupil_coverage(cx, cy, size_f).clamp(0.0, 1.0);
            let idx = ((y * size + x) * 4) as usize;
            buf[idx] = lerp_u8(r, pr, pupil_t);
            buf[idx + 1] = lerp_u8(g, pg, pupil_t);
            buf[idx + 2] = lerp_u8(b, pb, pupil_t);
            buf[idx + 3] = (a as f32 * eye_cov.clamp(0.0, 1.0)).round() as u8;
        }
    }
    (buf, size, size)
}

pub struct MenubarOptions {
    pub status_file: String,
    pub incidents_dir: String,
    pub poll_interval: Duration,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_status_json_round_trips_through_status_file() {
        let json = build_status_json(3, 1_000_000);
        let parsed: StatusFile = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.open_count, 3);
        assert_eq!(parsed.updated_unix, 1_000_000);
    }

    #[test]
    fn classify_health_is_unknown_without_a_status_file() {
        assert_eq!(classify_health(None, 1000, 30), HealthLevel::Unknown);
    }

    #[test]
    fn classify_health_is_unknown_when_status_is_stale() {
        let status = StatusFile { updated_unix: 1000, open_count: 0 };
        assert_eq!(classify_health(Some(&status), 1000 + 31, 30), HealthLevel::Unknown);
    }

    #[test]
    fn classify_health_ok_at_zero_open_incidents() {
        let status = StatusFile { updated_unix: 1000, open_count: 0 };
        assert_eq!(classify_health(Some(&status), 1005, 30), HealthLevel::Ok);
    }

    #[test]
    fn classify_health_warning_at_one_open_incident() {
        let status = StatusFile { updated_unix: 1000, open_count: 1 };
        assert_eq!(classify_health(Some(&status), 1005, 30), HealthLevel::Warning);
    }

    #[test]
    fn classify_health_critical_at_multiple_open_incidents() {
        let status = StatusFile { updated_unix: 1000, open_count: 2 };
        assert_eq!(classify_health(Some(&status), 1005, 30), HealthLevel::Critical);

        let many = StatusFile { updated_unix: 1000, open_count: 7 };
        assert_eq!(classify_health(Some(&many), 1005, 30), HealthLevel::Critical);
    }

    #[test]
    fn icon_rgba_has_correct_dimensions_and_is_fully_opaque_at_center_for_warning_and_critical() {
        for level in [HealthLevel::Warning, HealthLevel::Critical] {
            let (buf, w, h) = icon_rgba(level);
            assert_eq!(w, ICON_SIZE);
            assert_eq!(h, ICON_SIZE);
            assert_eq!(buf.len(), (w * h * 4) as usize);
            let center_idx = ((h / 2 * w + w / 2) * 4) as usize;
            assert_eq!(buf[center_idx + 3], 255, "{level:?} should be fully opaque at its center");
        }
    }

    #[test]
    fn icon_rgba_unknown_state_is_visible_but_distinct_from_ok() {
        let (unknown_buf, w, h) = icon_rgba(HealthLevel::Unknown);
        let (ok_buf, _, _) = icon_rgba(HealthLevel::Ok);
        let center_idx = ((h / 2 * w + w / 2) * 4) as usize;
        assert!(unknown_buf[center_idx + 3] > ok_buf[center_idx + 3], "Unknown should be more visible than Ok");
    }

    #[test]
    fn icon_rgba_ok_state_is_mostly_transparent() {
        let (buf, w, h) = icon_rgba(HealthLevel::Ok);
        let center_idx = ((h / 2 * w + w / 2) * 4) as usize;
        assert!(buf[center_idx + 3] < 100, "Ok state should read as faint/unobtrusive");
    }

    #[test]
    fn icon_rgba_corners_are_transparent_outside_the_eye() {
        let (buf, w, _h) = icon_rgba(HealthLevel::Critical);
        // Top-left corner pixel is outside the eye shape inscribed in the icon.
        assert_eq!(buf[3], 0);
        let _ = w;
    }

    #[test]
    fn is_inside_eye_is_wider_than_it_is_tall() {
        let size = ICON_SIZE as f32;
        let center = size / 2.0;
        // A point straight out horizontally from center, within the eye's
        // belly, should be inside; the same offset applied vertically
        // (well past the flatter top/bottom) should not be -- confirms
        // this reads as a horizontal almond, not a circle.
        assert!(is_inside_eye(center + 5.0, center, size));
        assert!(!is_inside_eye(center, center + 5.0, size));
    }

    #[test]
    fn is_inside_pupil_is_a_small_dot_at_the_center() {
        let size = ICON_SIZE as f32;
        let center = size / 2.0;
        assert!(is_inside_pupil(center, center, size));
        assert!(!is_inside_pupil(center + 5.0, center, size));
    }

    #[test]
    fn pupil_is_always_a_subset_of_the_eye() {
        // The pupil should never poke outside the eye outline it sits in.
        let size = ICON_SIZE as f32;
        let mut y = 0.0;
        while y < size {
            let mut x = 0.0;
            while x < size {
                if is_inside_pupil(x, y, size) {
                    assert!(is_inside_eye(x, y, size), "pupil point ({x}, {y}) escaped the eye outline");
                }
                x += 0.5;
            }
            y += 0.5;
        }
    }

    #[test]
    fn default_status_file_is_under_home() {
        assert!(default_status_file().ends_with(".vigil/status.json"));
    }

    #[test]
    fn write_status_then_read_status_round_trips() {
        let mut path = std::env::temp_dir();
        path.push(format!("vigil-menubar-test-{}.json", std::process::id()));
        write_status(path.to_str().unwrap(), 2);

        let status = read_status(&path).expect("just-written status file should parse");
        assert_eq!(status.open_count, 2);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn write_status_to_an_unwritable_path_does_not_panic() {
        // No such directory -- `std::fs::write` fails, and `write_status`
        // is documented to just log that, not propagate or panic.
        write_status("/nonexistent-dir-for-vigil-tests/status.json", 0);
    }

    #[test]
    fn read_status_returns_none_for_a_missing_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("vigil-menubar-missing-{}.json", std::process::id()));
        assert!(read_status(&path).is_none());
    }
}
