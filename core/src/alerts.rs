use crate::reader::Reading;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Warning,
    Critical,
}

impl Severity {
    pub fn label(&self) -> &'static str {
        match self {
            Severity::Warning => "Warning",
            Severity::Critical => "Critical",
        }
    }
}

const TEMP_WARNING_C: f64 = 45.0;
const TEMP_CRITICAL_C: f64 = 55.0;
// A spike must clear the recent baseline by this factor before it's worth
// surfacing -- normal load swings (browser tab opening, a CPU burst)
// shouldn't page the user, only genuinely abnormal draw.
const SPIKE_FACTOR: f64 = 2.5;
const EMA_ALPHA: f64 = 0.1;

/// Watches a stream of `Reading`s for genuinely alert-worthy transitions
/// (crossing a battery threshold, overheating, an abnormal power-draw
/// spike) and emits each one exactly once per crossing -- edge-triggered,
/// like network-monitor's `AnomalyDetector`, so a battery that sits at 4%
/// for an hour doesn't spam the alert list every second, only the moment
/// it first crosses.
pub struct BatteryAlertDetector {
    low_threshold: f64,
    critical_threshold: f64,
    armed_low: bool,
    armed_critical: bool,
    armed_temp: bool,
    power_ema: Option<f64>,
    armed_spike: bool,
}

impl BatteryAlertDetector {
    pub fn new(low_threshold: f64, critical_threshold: f64) -> Self {
        BatteryAlertDetector {
            low_threshold,
            critical_threshold,
            armed_low: true,
            armed_critical: true,
            armed_temp: true,
            power_ema: None,
            armed_spike: true,
        }
    }

    pub fn set_thresholds(&mut self, low: f64, critical: f64) {
        self.low_threshold = low;
        self.critical_threshold = critical;
    }

    pub fn observe(&mut self, r: &Reading) -> Vec<(Severity, String)> {
        let mut out = Vec::new();
        self.observe_capacity(r, &mut out);
        self.observe_temperature(r, &mut out);
        self.observe_power_spike(r, &mut out);
        out
    }

    fn observe_capacity(&mut self, r: &Reading, out: &mut Vec<(Severity, String)>) {
        let Some(cap) = r.capacity else { return };
        let discharging = r.status == "Discharging";

        if discharging && cap <= self.critical_threshold {
            if self.armed_critical {
                out.push((
                    Severity::Critical,
                    format!("Critical battery: {cap:.0}% remaining (threshold {:.0}%)", self.critical_threshold),
                ));
                self.armed_critical = false;
                self.armed_low = false;
            }
        } else if discharging && cap <= self.low_threshold {
            if self.armed_low {
                out.push((
                    Severity::Warning,
                    format!("Low battery: {cap:.0}% remaining (threshold {:.0}%)", self.low_threshold),
                ));
                self.armed_low = false;
            }
        } else {
            self.armed_low = true;
            self.armed_critical = true;
        }
    }

    fn observe_temperature(&mut self, r: &Reading, out: &mut Vec<(Severity, String)>) {
        let Some(temp) = r.temperature else { return };

        if temp >= TEMP_CRITICAL_C {
            if self.armed_temp {
                out.push((Severity::Critical, format!("High battery temperature: {temp:.1}°C")));
                self.armed_temp = false;
            }
        } else if temp >= TEMP_WARNING_C {
            if self.armed_temp {
                out.push((Severity::Warning, format!("Elevated battery temperature: {temp:.1}°C")));
                self.armed_temp = false;
            }
        } else {
            self.armed_temp = true;
        }
    }

    fn observe_power_spike(&mut self, r: &Reading, out: &mut Vec<(Severity, String)>) {
        let Some(power) = r.power else { return };

        let Some(ema) = self.power_ema else {
            self.power_ema = Some(power);
            return;
        };

        // A near-zero baseline (idle/fully charged) makes the ratio test
        // meaningless -- any nonzero draw would look like an "infinite"
        // spike. Only evaluate once the baseline itself is meaningful.
        if ema > 0.5 {
            if power > ema * SPIKE_FACTOR && self.armed_spike {
                out.push((Severity::Warning, format!("Power draw spike: {power:.1} W vs a recent baseline of {ema:.1} W")));
                self.armed_spike = false;
            } else if power < ema * 1.3 {
                self.armed_spike = true;
            }
        } else {
            self.armed_spike = true;
        }

        self.power_ema = Some(ema * (1.0 - EMA_ALPHA) + power * EMA_ALPHA);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reading(status: &str, capacity: f64) -> Reading {
        Reading { status: status.to_string(), capacity: Some(capacity), ..Reading::default() }
    }

    #[test]
    fn low_battery_fires_once_per_crossing() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        assert!(d.observe(&reading("Discharging", 50.0)).is_empty());
        let alerts = d.observe(&reading("Discharging", 19.0));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, Severity::Warning);
        // Still below threshold on the next tick -- must not re-fire.
        assert!(d.observe(&reading("Discharging", 18.0)).is_empty());
    }

    #[test]
    fn low_battery_rearms_after_recovering_above_threshold() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        d.observe(&reading("Discharging", 19.0));
        assert!(d.observe(&reading("Charging", 25.0)).is_empty());
        let alerts = d.observe(&reading("Discharging", 19.0));
        assert_eq!(alerts.len(), 1);
    }

    #[test]
    fn critical_takes_priority_and_suppresses_low_on_the_same_tick() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        d.observe(&reading("Discharging", 50.0));
        let alerts = d.observe(&reading("Discharging", 4.0));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, Severity::Critical);
    }

    #[test]
    fn charging_never_triggers_low_battery() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        assert!(d.observe(&reading("Charging", 4.0)).is_empty());
    }

    #[test]
    fn temperature_warning_and_critical_tiers() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        let mut r = reading("Discharging", 80.0);
        r.temperature = Some(50.0);
        let alerts = d.observe(&r);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, Severity::Warning);

        r.temperature = Some(30.0); // cools down, re-arms
        d.observe(&r);
        r.temperature = Some(60.0);
        let alerts = d.observe(&r);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, Severity::Critical);
    }

    #[test]
    fn power_spike_detected_against_a_warm_baseline() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        let mut r = reading("Discharging", 80.0);
        r.power = Some(10.0);
        for _ in 0..30 {
            d.observe(&r); // let the EMA settle near 10W
        }
        r.power = Some(40.0); // 4x baseline
        let alerts = d.observe(&r);
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].0, Severity::Warning);
    }

    #[test]
    fn no_spike_alert_from_a_near_zero_idle_baseline() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        let mut r = reading("Full", 100.0);
        r.power = Some(0.05);
        for _ in 0..10 {
            d.observe(&r);
        }
        r.power = Some(2.0); // technically 40x, but baseline is negligible
        assert!(d.observe(&r).is_empty());
    }

    #[test]
    fn missing_fields_are_silently_skipped_not_fabricated() {
        let mut d = BatteryAlertDetector::new(20.0, 5.0);
        let r = Reading::default();
        assert!(d.observe(&r).is_empty());
    }
}
