use std::path::Path;
use std::process::Command;

// package-0 on this dev machine (confirmed: `cat
// /sys/class/powercap/intel-rapl:0/name` -> "package-0") -- the typical
// zone index for the whole-CPU-package domain, though not guaranteed on
// every system (multi-socket machines, or AMD hardware which has no
// Intel RAPL interface at all).
const RAPL_PACKAGE_ZONE: &str = "/sys/class/powercap/intel-rapl:0";
const SAMPLE_SECONDS: u32 = 1;

/// Whether this machine exposes an Intel RAPL package-power zone at all
/// -- checked before offering the UI action, same "don't offer a button
/// for something that can't work here" rule as Charging's threshold
/// sliders. Doesn't confirm *readability* (that needs root, checked
/// below), just presence.
pub fn rapl_available() -> bool {
    Path::new(RAPL_PACKAGE_ZONE).join("energy_uj").exists()
}

/// Reads real CPU package power via Intel RAPL (`energy_uj`, sampled
/// twice one second apart), which needs root to read on this system
/// (confirmed: permission denied as a normal user) -- `pkexec`-gated,
/// one-shot, manual-only, same rule as `processes::run_powertop_scan`:
/// never called from a poll loop.
///
/// This is CPU package power specifically, not whole-system power (no
/// display, SSD, etc.) -- callers should label it as such, not as
/// "system power" or "total draw".
/// The counter wraps at `max_range` rather than saturating -- if `after <
/// before` it wrapped at least once during the sample window, so the real
/// delta is the distance to `max_range` plus however far past zero it's
/// gone, not a negative number.
fn energy_delta_uj(before: u64, after: u64, max_range: u64) -> u64 {
    if after < before { after + max_range - before } else { after - before }
}

pub fn read_package_power_watts() -> Result<f64, String> {
    if !rapl_available() {
        return Err("No Intel RAPL package-power zone on this system".to_string());
    }
    // Elevated once for both samples (rather than two separate pkexec
    // calls) so the 1s gap between them is exact, not stretched by a
    // second password prompt. The shell script only reads raw values --
    // the wraparound-safe delta itself is computed in Rust below, where
    // it's unit-testable.
    let script = format!(
        "a=$(cat {zone}/energy_uj) && m=$(cat {zone}/max_energy_range_uj) && sleep {secs} && b=$(cat {zone}/energy_uj) && echo \"$a $m $b\"",
        zone = RAPL_PACKAGE_ZONE,
        secs = SAMPLE_SECONDS,
    );
    let output = Command::new("pkexec")
        .arg("sh")
        .arg("-c")
        .arg(&script)
        .output()
        .map_err(|e| format!("failed to run pkexec: {e}"))?;
    if !output.status.success() {
        return Err("RAPL read failed (cancelled, or rejected by polkit)".to_string());
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut parts = text.split_whitespace();
    let (before, max_range, after) = match (parts.next(), parts.next(), parts.next()) {
        (Some(a), Some(m), Some(b)) => match (a.parse::<u64>(), m.parse::<u64>(), b.parse::<u64>()) {
            (Ok(a), Ok(m), Ok(b)) => (a, m, b),
            _ => return Err("unexpected RAPL output".to_string()),
        },
        _ => return Err("unexpected RAPL output".to_string()),
    };
    let delta_uj = energy_delta_uj(before, after, max_range) as f64;
    Ok(delta_uj / 1_000_000.0 / SAMPLE_SECONDS as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn energy_delta_normal_increase() {
        assert_eq!(energy_delta_uj(1_000, 3_000, 1_000_000), 2_000);
    }

    #[test]
    fn energy_delta_handles_counter_wraparound() {
        // Wrapped mid-sample: 1_000 left to reach max_range, then 500 more past zero.
        assert_eq!(energy_delta_uj(999_000, 500, 1_000_000), 1_500);
    }
}
