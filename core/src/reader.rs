use std::path::{Path, PathBuf};

/// Returns all BAT* power supplies found under /sys/class/power_supply,
/// sorted by name.
pub fn find_battery_paths() -> Vec<PathBuf> {
    let base = Path::new("/sys/class/power_supply");
    let mut found: Vec<PathBuf> = match std::fs::read_dir(base) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("BAT"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    found.sort();
    found
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reading {
    pub status: String,
    pub capacity: Option<f64>,
    pub voltage: Option<f64>,
    pub design_voltage: Option<f64>,
    pub current_a: Option<f64>,
    pub charging_current: f64,
    pub discharging_current: f64,
    pub power: Option<f64>,
    pub temperature: Option<f64>,
    pub charge_now: Option<f64>,
    pub charge_full: Option<f64>,
    pub design_capacity: Option<f64>,
    pub energy_now: Option<f64>,
    pub energy_full: Option<f64>,
    pub energy_design: Option<f64>,
    pub health: Option<f64>,
    pub health_status: String,
    pub cycle: Option<f64>,
    pub charge_limit: Option<f64>,
    pub time_remaining_str: String,
    pub short_time_str: String,
    pub manufacturer: String,
    pub model: String,
    pub technology: String,
}

impl Reading {
    fn no_battery() -> Self {
        Reading {
            status: "No Battery".into(),
            capacity: None,
            voltage: None,
            design_voltage: None,
            current_a: None,
            charging_current: 0.0,
            discharging_current: 0.0,
            power: None,
            temperature: None,
            charge_now: None,
            charge_full: None,
            design_capacity: None,
            energy_now: None,
            energy_full: None,
            energy_design: None,
            health: None,
            health_status: "Unknown".into(),
            cycle: None,
            charge_limit: None,
            time_remaining_str: "No battery detected".into(),
            short_time_str: "—".into(),
            manufacturer: "—".into(),
            model: "—".into(),
            technology: "—".into(),
        }
    }
}

pub struct BatteryReader {
    path: Option<PathBuf>,
}

impl BatteryReader {
    pub fn new(path: Option<PathBuf>) -> Self {
        BatteryReader { path }
    }

    fn read(&self, name: &str) -> Option<String> {
        let path = self.path.as_ref()?;
        std::fs::read_to_string(path.join(name))
            .ok()
            .map(|s| s.trim().to_string())
    }

    fn num(&self, name: &str) -> Option<f64> {
        self.read(name)?.parse::<f64>().ok()
    }

    pub fn get(&self) -> Reading {
        let Some(_) = self.path.as_ref() else {
            return Reading::no_battery();
        };

        let status = self.read("status").unwrap_or_else(|| "Unknown".into());

        let capacity = self.num("capacity");
        let voltage_raw = self.num("voltage_now");
        let current_raw = self.num("current_now");
        let temp_raw = self.num("temp");

        let charge_now_raw = self.num("charge_now");
        let charge_full_raw = self.num("charge_full");
        let charge_design_raw = self.num("charge_full_design");
        let cycle = self.num("cycle_count");

        let charge_limit = self.num("charge_control_end_threshold");

        let energy_now_raw = self.num("energy_now");
        let energy_full_raw = self.num("energy_full");
        let energy_design_raw = self.num("energy_full_design");

        let voltage = voltage_raw.map(|v| v / 1_000_000.0);
        let design_voltage_raw = self.num("voltage_min_design");
        let design_voltage = design_voltage_raw.map(|v| v / 1_000_000.0);

        let current_a = current_raw.map(|v| v / 1_000_000.0);
        let temperature = temp_raw.map(|v| v / 10.0);

        let charge_now = charge_now_raw.map(|v| v / 1_000_000.0);
        let charge_full = charge_full_raw.map(|v| v / 1_000_000.0);
        let design_capacity = charge_design_raw.map(|v| v / 1_000_000.0);

        let native_energy_now = energy_now_raw.map(|v| v / 1_000_000.0);
        let native_energy_full = energy_full_raw.map(|v| v / 1_000_000.0);
        let native_energy_design = energy_design_raw.map(|v| v / 1_000_000.0);

        // Only derive Wh from Ah * V when we have a *real* voltage reading
        // (current or design) -- silently assuming 1V would produce a
        // wildly wrong energy figure for anything other than a 1-cell pack.
        let known_voltage = design_voltage.or(voltage);

        let energy_now = native_energy_now.or(match (charge_now, known_voltage) {
            (Some(c), Some(v)) => Some(c * v),
            _ => None,
        });
        let energy_full = native_energy_full.or(match (charge_full, known_voltage) {
            (Some(c), Some(v)) => Some(c * v),
            _ => None,
        });
        let energy_design = native_energy_design.or(match (design_capacity, known_voltage) {
            (Some(c), Some(v)) => Some(c * v),
            _ => None,
        });

        let charging_current = if status == "Charging" {
            current_a.unwrap_or(0.0)
        } else {
            0.0
        };
        let discharging_current = if status == "Discharging" {
            current_a.unwrap_or(0.0)
        } else {
            0.0
        };

        let power = match (voltage, current_a) {
            (Some(v), Some(c)) => Some(v * c),
            _ => None,
        };

        // Prefer the Ah-based ratio (matches what most tools report), but
        // fall back to the Wh-based ratio for batteries that only expose
        // energy_* nodes (common on some ARM / newer ThinkPad firmwares).
        let health = if let (Some(cf), Some(dc)) = (charge_full, design_capacity) {
            if dc != 0.0 {
                Some(cf / dc * 100.0)
            } else {
                None
            }
        } else if let (Some(ef), Some(ed)) = (energy_full, energy_design) {
            if ed != 0.0 {
                Some(ef / ed * 100.0)
            } else {
                None
            }
        } else {
            None
        };

        let health_status = match health {
            Some(h) if h >= 80.0 => "Good",
            Some(h) if h >= 60.0 => "Fair",
            Some(h) if h >= 40.0 => "Poor",
            Some(_) => "Critical",
            None => "Unknown",
        }
        .to_string();

        // Dispatch on status first (it's the authoritative signal from the
        // kernel), then use power/energy only to compute the estimate
        // within that status.
        let mut time_remaining_str = "Calculating...".to_string();
        let mut short_time_str = "—".to_string();

        match status.as_str() {
            "Discharging" => {
                if let (Some(p), Some(en)) = (power, energy_now) {
                    if p > 0.01 {
                        let hrs = en / p;
                        let m = ((hrs.fract()) * 60.0) as i64;
                        time_remaining_str = format!("{}h {}m remaining", hrs as i64, m);
                        short_time_str = format!("{}h {}m", hrs as i64, m);
                    }
                }
            }
            "Charging" => {
                if let (Some(p), Some(ef), Some(en)) = (power, energy_full, energy_now) {
                    if p > 0.01 {
                        let hrs = (ef - en).max(0.0) / p;
                        let m = ((hrs.fract()) * 60.0) as i64;
                        time_remaining_str = format!("{}h {}m until full", hrs as i64, m);
                        short_time_str = format!("{}h {}m", hrs as i64, m);
                    }
                }
            }
            "Full" => {
                time_remaining_str = "Fully Charged".to_string();
                short_time_str = "Full".to_string();
            }
            "Not charging" => {
                // Plugged in but not drawing current -- typically a charge
                // threshold (charge_limit above) holding it below 100% to
                // preserve battery health, common on ThinkPads and similar.
                if let Some(limit) = charge_limit {
                    time_remaining_str = format!("Charge limit reached ({:.0}%)", limit);
                    short_time_str = format!("Limit {:.0}%", limit);
                } else {
                    time_remaining_str = "Plugged in, not charging".to_string();
                    short_time_str = "Idle".to_string();
                }
            }
            _ => {}
        }

        Reading {
            status,
            capacity,
            voltage,
            design_voltage,
            current_a,
            charging_current,
            discharging_current,
            power,
            temperature,
            charge_now,
            charge_full,
            design_capacity,
            energy_now,
            energy_full,
            energy_design,
            health,
            health_status,
            cycle,
            charge_limit,
            time_remaining_str,
            short_time_str,
            manufacturer: self.read("manufacturer").unwrap_or_else(|| "Unknown".into()),
            model: self.read("model_name").unwrap_or_else(|| "Unknown".into()),
            technology: self.read("technology").unwrap_or_else(|| "Unknown".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn make_battery_dir(fields: &BTreeMap<&str, String>) -> tempfile_dir::TempDir {
        let dir = tempfile_dir::TempDir::new();
        let bat = dir.path().join("BAT0");
        std::fs::create_dir_all(&bat).unwrap();
        for (name, value) in fields {
            std::fs::write(bat.join(name), value).unwrap();
        }
        dir
    }

    fn base_fields() -> BTreeMap<&'static str, String> {
        let mut m = BTreeMap::new();
        m.insert("voltage_now", "12000000".to_string());
        m.insert("voltage_min_design", "11000000".to_string());
        m.insert("charge_now", "2500000".to_string());
        m.insert("charge_full", "5000000".to_string());
        m.insert("charge_full_design", "5500000".to_string());
        m.insert("cycle_count", "42".to_string());
        m.insert("manufacturer", "Acme".to_string());
        m.insert("model_name", "X1".to_string());
        m.insert("technology", "Li-ion".to_string());
        m
    }

    fn reading_with(overrides: &[(&str, &str)]) -> Reading {
        let mut fields = base_fields();
        for (k, v) in overrides {
            fields.insert(k, v.to_string());
        }
        let dir = make_battery_dir(&fields);
        BatteryReader::new(Some(dir.path().join("BAT0"))).get()
    }

    fn reading_without(remove: &[&str], overrides: &[(&str, &str)]) -> Reading {
        let mut fields = base_fields();
        for k in remove {
            fields.remove(k);
        }
        for (k, v) in overrides {
            fields.insert(k, v.to_string());
        }
        let dir = make_battery_dir(&fields);
        BatteryReader::new(Some(dir.path().join("BAT0"))).get()
    }

    #[test]
    fn no_battery_returns_placeholder() {
        let data = BatteryReader::new(None).get();
        assert_eq!(data.status, "No Battery");
        assert_eq!(data.capacity, None);
        assert_eq!(data.time_remaining_str, "No battery detected");
    }

    #[test]
    fn discharging_computes_remaining_time() {
        let data = reading_with(&[
            ("status", "Discharging"),
            ("capacity", "50"),
            ("current_now", "1000000"),
        ]);
        assert!(data.time_remaining_str.contains("remaining"));
        assert!((data.discharging_current - 1.0).abs() < 1e-9);
        assert_eq!(data.charging_current, 0.0);
    }

    #[test]
    fn charging_computes_time_until_full() {
        let data = reading_with(&[
            ("status", "Charging"),
            ("capacity", "50"),
            ("current_now", "1000000"),
        ]);
        assert!(data.time_remaining_str.contains("until full"));
        assert!((data.charging_current - 1.0).abs() < 1e-9);
    }

    #[test]
    fn full_status_reports_fully_charged() {
        let data = reading_with(&[("status", "Full"), ("capacity", "100"), ("current_now", "0")]);
        assert_eq!(data.time_remaining_str, "Fully Charged");
        assert_eq!(data.short_time_str, "Full");
    }

    #[test]
    fn full_with_residual_current_still_reports_fully_charged() {
        let data = reading_with(&[("status", "Full"), ("capacity", "100"), ("current_now", "50000")]);
        assert_eq!(data.time_remaining_str, "Fully Charged");
    }

    #[test]
    fn not_charging_with_limit_reports_the_limit() {
        let data = reading_with(&[
            ("status", "Not charging"),
            ("capacity", "80"),
            ("current_now", "0"),
            ("charge_control_end_threshold", "80"),
        ]);
        assert_eq!(data.time_remaining_str, "Charge limit reached (80%)");
        assert_eq!(data.short_time_str, "Limit 80%");
    }

    #[test]
    fn not_charging_without_limit_reports_generic_idle() {
        let data = reading_with(&[("status", "Not charging"), ("capacity", "100"), ("current_now", "0")]);
        assert_eq!(data.time_remaining_str, "Plugged in, not charging");
        assert_eq!(data.short_time_str, "Idle");
    }

    #[test]
    fn health_uses_ah_ratio_when_available() {
        let data = reading_with(&[("charge_full", "4400000"), ("charge_full_design", "5500000")]);
        assert!((data.health.unwrap() - 80.0).abs() < 1e-6);
        assert_eq!(data.health_status, "Good");
    }

    #[test]
    fn health_falls_back_to_wh_ratio_without_charge_nodes() {
        let data = reading_without(
            &["charge_full", "charge_full_design"],
            &[("energy_full", "44000000"), ("energy_full_design", "55000000")],
        );
        assert!((data.health.unwrap() - 80.0).abs() < 1e-6);
    }

    #[test]
    fn missing_voltage_does_not_fabricate_energy() {
        let data = reading_without(
            &["voltage_now", "voltage_min_design"],
            &[("status", "Discharging"), ("capacity", "50"), ("current_now", "1000000")],
        );
        assert_eq!(data.voltage, None);
        assert_eq!(data.energy_now, None);
    }

    #[test]
    fn missing_sysfs_file_reads_as_none_not_error() {
        let mut fields = BTreeMap::new();
        fields.insert("status", "Discharging".to_string());
        let dir = make_battery_dir(&fields);
        let data = BatteryReader::new(Some(dir.path().join("BAT0"))).get();
        assert_eq!(data.capacity, None);
    }
}

#[cfg(test)]
mod tempfile_dir {
    use std::path::{Path, PathBuf};

    pub struct TempDir(PathBuf);

    impl TempDir {
        pub fn new() -> Self {
            let dir = std::env::temp_dir().join(format!(
                "battery-core-test-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempDir(dir)
        }

        pub fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}
