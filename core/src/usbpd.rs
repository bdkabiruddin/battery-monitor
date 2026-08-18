use std::path::Path;

#[derive(Debug, Clone, PartialEq)]
pub struct UsbPdInfo {
    pub usb_type: String,
    pub voltage_v: Option<f64>,
    pub current_a: Option<f64>,
    pub max_current_a: Option<f64>,
    pub watts: Option<f64>,
}

/// Reads the active USB-C Power Delivery source, if any, from
/// `/sys/class/power_supply/*` -- kernel UCSI/typec drivers expose each
/// negotiated PD source as its own `power_supply` device (commonly named
/// `ucsi-source-psy-*`), with real negotiated voltage/current, not
/// something this app estimates. Returns `None` if no such device exists
/// or none is currently online (e.g. running on battery only, or a
/// non-PD charger) -- never fabricated.
pub fn read_active_usb_pd() -> Option<UsbPdInfo> {
    read_active_usb_pd_under(Path::new("/sys/class/power_supply"))
}

fn read_active_usb_pd_under(base: &Path) -> Option<UsbPdInfo> {
    let entries = std::fs::read_dir(base).ok()?;
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        let online = std::fs::read_to_string(path.join("online")).ok();
        if online.as_deref().map(|s| s.trim()) != Some("1") {
            continue;
        }
        let Some(usb_type) = std::fs::read_to_string(path.join("usb_type")).ok() else {
            continue;
        };
        let usb_type = usb_type.trim().to_string();
        if !usb_type.contains("PD") {
            continue;
        }
        let read_micro = |name: &str| -> Option<f64> {
            std::fs::read_to_string(path.join(name)).ok()?.trim().parse::<f64>().ok().map(|v| v / 1_000_000.0)
        };
        let voltage_v = read_micro("voltage_now");
        let current_a = read_micro("current_now");
        let max_current_a = read_micro("current_max");
        let watts = match (voltage_v, current_a) {
            (Some(v), Some(c)) => Some(v * c),
            _ => None,
        };
        return Some(UsbPdInfo { usb_type, voltage_v, current_a, max_current_a, watts });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_supply_dir(base: &Path, name: &str, fields: &[(&str, &str)]) -> std::path::PathBuf {
        let path = base.join(name);
        std::fs::create_dir_all(&path).unwrap();
        for (k, v) in fields {
            std::fs::write(path.join(k), v).unwrap();
        }
        path
    }

    fn temp_base(suffix: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("battery-core-usbpd-test-{}-{}", std::process::id(), suffix));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn finds_an_online_pd_source_and_scales_micro_units() {
        let base = temp_base("online-pd");
        make_supply_dir(
            &base,
            "ucsi-source-psy-0",
            &[
                ("online", "1"),
                ("usb_type", "PD"),
                ("voltage_now", "9000000"),
                ("current_now", "2000000"),
                ("current_max", "3000000"),
            ],
        );

        let found = read_active_usb_pd_under(&base).unwrap();
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(found.usb_type, "PD");
        assert_eq!(found.voltage_v, Some(9.0));
        assert_eq!(found.current_a, Some(2.0));
        assert_eq!(found.max_current_a, Some(3.0));
        assert_eq!(found.watts, Some(18.0)); // 9V * 2A
    }

    #[test]
    fn skips_an_offline_device() {
        let base = temp_base("offline");
        make_supply_dir(&base, "ucsi-source-psy-0", &[("online", "0"), ("usb_type", "PD")]);

        let found = read_active_usb_pd_under(&base);
        std::fs::remove_dir_all(&base).ok();

        assert!(found.is_none());
    }

    #[test]
    fn skips_a_non_pd_online_source() {
        let base = temp_base("non-pd");
        make_supply_dir(&base, "usb-charger", &[("online", "1"), ("usb_type", "SDP")]);

        let found = read_active_usb_pd_under(&base);
        std::fs::remove_dir_all(&base).ok();

        assert!(found.is_none());
    }

    #[test]
    fn missing_current_leaves_watts_unset_rather_than_fabricated() {
        let base = temp_base("no-current");
        make_supply_dir(&base, "ucsi-source-psy-0", &[("online", "1"), ("usb_type", "PD"), ("voltage_now", "9000000")]);

        let found = read_active_usb_pd_under(&base).unwrap();
        std::fs::remove_dir_all(&base).ok();

        assert_eq!(found.voltage_v, Some(9.0));
        assert_eq!(found.current_a, None);
        assert_eq!(found.watts, None);
    }
}
