use std::process::Command;

/// Real, live-tested via `power-profiles-daemon` (`powerprofilesctl`) --
/// present and active on Fedora/Ubuntu/most GNOME systems. Degrades to an
/// empty/`None` result, never a fabricated profile, if the daemon isn't
/// installed or running.
pub fn list_profiles() -> Vec<String> {
    let Ok(output) = Command::new("powerprofilesctl").arg("list").output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    parse_profile_list(&String::from_utf8_lossy(&output.stdout))
}

/// Profile names from `powerprofilesctl list` output -- each profile
/// starts a line with either `  name:` or, for the active one, `* name:`.
fn parse_profile_list(text: &str) -> Vec<String> {
    text.lines()
        .filter_map(|line| {
            let trimmed = line.trim_start_matches('*').trim();
            if line.starts_with(' ') || line.starts_with('*') {
                trimmed.strip_suffix(':').map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        .collect()
}

pub fn active_profile() -> Option<String> {
    let output = Command::new("powerprofilesctl").arg("get").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let name = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Session-level via `power-profiles-daemon`'s own polkit policy (no
/// `pkexec` needed on a normal desktop session) -- unlike the
/// charge-threshold sysfs writes, which do need root.
pub fn set_profile(name: &str) -> Result<(), String> {
    let status = Command::new("powerprofilesctl")
        .arg("set")
        .arg(name)
        .status()
        .map_err(|e| format!("failed to run powerprofilesctl: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("powerprofilesctl set {name} failed"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_powerprofilesctl_list_output() {
        let sample = "  performance:\n    Driver:     intel_pstate\n    Degraded:   no\n\n  balanced:\n    Driver:     intel_pstate\n\n* power-saver:\n    Driver:     intel_pstate\n";
        let profiles = parse_profile_list(sample);
        assert_eq!(profiles, vec!["performance", "balanced", "power-saver"]);
    }

    #[test]
    fn empty_output_yields_no_profiles() {
        assert!(parse_profile_list("").is_empty());
    }
}
