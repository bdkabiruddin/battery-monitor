use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub struct PeripheralDevice {
    pub name: String,
    pub manufacturer: Option<String>,
    pub status: String,
    /// Some HID++/Bluetooth peripherals only expose a coarse
    /// `capacity_level` string (Full/High/Normal/Low/Critical/Unknown),
    /// not a real percentage -- `None` here rather than guessing a number
    /// from that enum.
    pub capacity_percent: Option<f64>,
}

/// Scans `/sys/class/power_supply/*` for `type=Battery` devices other
/// than the laptop's own internal battery/batteries (`exclude`, e.g. the
/// `BAT0` path `BatteryReader` already reads for the Dashboard) --
/// wireless mice/keyboards with their own fuel gauge (Logitech HID++
/// receivers, some Bluetooth HID devices) show up here as their own
/// `power_supply` node. This only finds devices the kernel already
/// created a sysfs node for; UPower's D-Bus interface can enumerate a
/// few more (pure-Bluetooth battery reporting with no sysfs backing) but
/// that needs a D-Bus client dependency this crate doesn't have yet --
/// documented as a known gap, not silently pretended-complete.
pub fn read_peripheral_devices(exclude: &[PathBuf]) -> Vec<PeripheralDevice> {
    read_peripheral_devices_under(Path::new("/sys/class/power_supply"), exclude)
}

fn read_peripheral_devices_under(base: &Path, exclude: &[PathBuf]) -> Vec<PeripheralDevice> {
    let Ok(entries) = std::fs::read_dir(base) else { return Vec::new() };

    let mut out = Vec::new();
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if exclude.iter().any(|p| p == &path) {
            continue;
        }
        let Some(device_type) = read_trimmed(&path.join("type")) else { continue };
        if device_type != "Battery" {
            continue;
        }

        let name = read_trimmed(&path.join("model_name"))
            .or_else(|| entry.file_name().to_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "Unknown device".to_string());
        let manufacturer = read_trimmed(&path.join("manufacturer"));
        let status = read_trimmed(&path.join("status"))
            .or_else(|| read_trimmed(&path.join("capacity_level")))
            .unwrap_or_else(|| "Unknown".to_string());
        let capacity_percent = read_trimmed(&path.join("capacity")).and_then(|s| s.parse().ok());

        out.push(PeripheralDevice { name, manufacturer, status, capacity_percent });
    }
    out
}

fn read_trimmed(path: &Path) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_supply_dir(base: &Path, name: &str, fields: &[(&str, &str)]) -> PathBuf {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        for (k, v) in fields {
            std::fs::write(path.join(k), v).unwrap();
        }
        path
    }

    fn temp_base(suffix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("battery-core-devices-test-{}-{}", std::process::id(), suffix));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_a_battery_type_peripheral_with_no_numeric_capacity() {
        let base = temp_base("mouse");
        make_supply_dir(
            &base,
            "hidpp_battery_7",
            &[("type", "Battery"), ("model_name", "Wireless Mouse MX Master 3"), ("manufacturer", "Logitech"), ("status", "Charging"), ("capacity_level", "Unknown")],
        );

        let found = read_peripheral_devices_under(&base, &[]);
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].name, "Wireless Mouse MX Master 3");
        assert_eq!(found[0].manufacturer.as_deref(), Some("Logitech"));
        assert_eq!(found[0].status, "Charging");
        assert_eq!(found[0].capacity_percent, None); // never guessed from capacity_level
    }

    #[test]
    fn skips_non_battery_type_supplies() {
        let base = temp_base("mains");
        make_supply_dir(&base, "AC", &[("type", "Mains"), ("online", "1")]);
        let found = read_peripheral_devices_under(&base, &[]);
        std::fs::remove_dir_all(&base).ok();
        assert!(found.is_empty());
    }

    #[test]
    fn excludes_the_given_paths() {
        let base = temp_base("exclude");
        let bat0 = make_supply_dir(&base, "BAT0", &[("type", "Battery"), ("model_name", "Internal"), ("status", "Full"), ("capacity", "100")]);
        make_supply_dir(&base, "hidpp_battery_7", &[("type", "Battery"), ("model_name", "Mouse"), ("status", "Discharging")]);

        let all = read_peripheral_devices_under(&base, &[]);
        let filtered = read_peripheral_devices_under(&base, &[bat0]);
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(all.len(), 2);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "Mouse");
    }

    #[test]
    fn reads_a_real_numeric_capacity_when_present() {
        let base = temp_base("numeric");
        make_supply_dir(&base, "hid_headset", &[("type", "Battery"), ("model_name", "Headset"), ("status", "Discharging"), ("capacity", "63")]);
        let found = read_peripheral_devices_under(&base, &[]);
        std::fs::remove_dir_all(&base).ok();
        assert_eq!(found[0].capacity_percent, Some(63.0));
    }
}
