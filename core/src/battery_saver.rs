use std::process::Command;

const SCHEMA: &str = "org.gnome.settings-daemon.plugins.power";
const KEY: &str = "power-saver-profile-on-low-battery";

/// Real GNOME Settings Daemon setting: automatically switches to the
/// power-saver profile when the battery gets low. `None` if `gsettings`
/// or the schema isn't available (non-GNOME desktop) -- never guessed.
pub fn auto_battery_saver_enabled() -> Option<bool> {
    let output = Command::new("gsettings").args(["get", SCHEMA, KEY]).output().ok()?;
    if !output.status.success() {
        return None;
    }
    parse_gsettings_bool(&String::from_utf8_lossy(&output.stdout))
}

fn parse_gsettings_bool(text: &str) -> Option<bool> {
    match text.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Per-user gsettings key -- no root/pkexec needed, unlike the
/// charge-threshold sysfs writes.
pub fn set_auto_battery_saver(enabled: bool) -> Result<(), String> {
    let status = Command::new("gsettings")
        .args(["set", SCHEMA, KEY, if enabled { "true" } else { "false" }])
        .status()
        .map_err(|e| format!("failed to run gsettings: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err("gsettings set failed".to_string())
    }
}

/// The real low/critical battery percentage thresholds UPower is
/// configured with (`/etc/UPower/UPower.conf`) -- read-only here (editing
/// requires a root file write plus restarting `upower.service`, out of
/// scope for now). `(None, None)` if the file can't be read or the keys
/// aren't set (commented out means "use UPower's own compiled-in
/// default" -- not something this function guesses at).
pub fn upower_low_critical_thresholds() -> (Option<f64>, Option<f64>) {
    let Ok(text) = std::fs::read_to_string("/etc/UPower/UPower.conf") else {
        return (None, None);
    };
    parse_upower_conf(&text)
}

fn parse_upower_conf(text: &str) -> (Option<f64>, Option<f64>) {
    let mut low = None;
    let mut critical = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if let Some(v) = line.strip_prefix("PercentageLow=") {
            low = v.trim().parse().ok();
        } else if let Some(v) = line.strip_prefix("PercentageCritical=") {
            critical = v.trim().parse().ok();
        }
    }
    (low, critical)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_gsettings_booleans() {
        assert_eq!(parse_gsettings_bool("true\n"), Some(true));
        assert_eq!(parse_gsettings_bool("false\n"), Some(false));
        assert_eq!(parse_gsettings_bool("garbage"), None);
    }

    #[test]
    fn parses_real_upower_conf_fragment() {
        let sample = "\
[UPower]
UsePercentageForPolicy=true
# comment line, ignored, must not shadow the real value below
#PercentageLow=10
PercentageLow=20
PercentageCritical=5
PercentageAction=2
";
        assert_eq!(parse_upower_conf(sample), (Some(20.0), Some(5.0)));
    }

    #[test]
    fn missing_keys_yield_none() {
        assert_eq!(parse_upower_conf("[UPower]\n"), (None, None));
    }
}
