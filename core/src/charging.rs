use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct ChargeThresholds {
    pub supported: bool,
    pub start: Option<f64>,
    pub end: Option<f64>,
}

fn start_path(battery_path: &Path) -> PathBuf {
    battery_path.join("charge_control_start_threshold")
}
fn end_path(battery_path: &Path) -> PathBuf {
    battery_path.join("charge_control_end_threshold")
}

/// Reads `charge_control_start_threshold`/`charge_control_end_threshold`
/// -- a real but hardware-dependent kernel ABI (ThinkPads, some Lenovo/
/// Huawei/Asus laptops via their ACPI drivers; not present on most
/// desktops or hardware without vendor charge-limiting support).
/// `supported: false` when the sysfs nodes don't exist on this machine at
/// all -- the honest answer, not a guess either way.
pub fn read_charge_thresholds(battery_path: Option<&Path>) -> ChargeThresholds {
    let Some(path) = battery_path else {
        return ChargeThresholds { supported: false, start: None, end: None };
    };
    let start = std::fs::read_to_string(start_path(path)).ok();
    let end = std::fs::read_to_string(end_path(path)).ok();
    let supported = start.is_some() || end.is_some();
    ChargeThresholds {
        supported,
        start: start.and_then(|s| s.trim().parse().ok()),
        end: end.and_then(|s| s.trim().parse().ok()),
    }
}

/// Writes both thresholds via `pkexec` -- these sysfs nodes are root-only
/// on every driver that exposes them, unlike the power-profile/
/// battery-saver settings above. Manual-only (an explicit "Apply" click),
/// never from a poll loop, same rule as `processes::run_powertop_scan`.
///
/// Not live-verified against real hardware in this dev environment (no
/// machine with `charge_control_*_threshold` support was available) --
/// implemented directly from the kernel's documented ABI
/// (`Documentation/ABI/testing/sysfs-class-power`), not guessed.
pub fn set_charge_thresholds(battery_path: &Path, start: u8, end: u8) -> Result<(), String> {
    if start >= end {
        return Err("start threshold must be less than end threshold".to_string());
    }
    let script = format!(
        "echo {start} > {start_path} && echo {end} > {end_path}",
        start_path = start_path(battery_path).display(),
        end_path = end_path(battery_path).display(),
    );
    let status = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .status()
        .map_err(|e| format!("failed to run pkexec: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("writing charge thresholds failed (cancelled, or rejected by the driver)".to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_when_battery_path_is_none() {
        let t = read_charge_thresholds(None);
        assert!(!t.supported);
        assert_eq!(t.start, None);
        assert_eq!(t.end, None);
    }

    #[test]
    fn unsupported_when_no_threshold_files_exist() {
        let dir = std::env::temp_dir().join(format!("battery-core-charging-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = read_charge_thresholds(Some(&dir));
        std::fs::remove_dir_all(&dir).ok();
        assert!(!t.supported);
    }

    #[test]
    fn supported_and_parsed_when_files_exist() {
        let dir = std::env::temp_dir().join(format!("battery-core-charging-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(start_path(&dir), "40").unwrap();
        std::fs::write(end_path(&dir), "80").unwrap();
        let t = read_charge_thresholds(Some(&dir));
        std::fs::remove_dir_all(&dir).ok();
        assert!(t.supported);
        assert_eq!(t.start, Some(40.0));
        assert_eq!(t.end, Some(80.0));
    }

    #[test]
    fn rejects_start_greater_than_or_equal_to_end() {
        let dir = std::env::temp_dir().join(format!("battery-core-charging-test3-{}", std::process::id()));
        assert!(set_charge_thresholds(&dir, 80, 40).is_err());
        assert!(set_charge_thresholds(&dir, 50, 50).is_err());
    }
}
