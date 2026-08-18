/// Signed wattage while charging/discharging, falling back to percentage
/// otherwise so the label doesn't just go blank -- ported 1:1 from the
/// companion GNOME Shell extension's `formatLabel.js` (the app's tray/
/// panel presence before this Rust rewrite), so the new tray icon reads
/// exactly the way users of the old extension are used to: "+15.2W"
/// while charging, "-8.4W" while discharging, "100%" when Full, else
/// "<capacity>%".
pub fn format_tray_label(status: &str, power: Option<f64>, capacity: Option<f64>) -> String {
    if status == "Charging" {
        if let Some(p) = power {
            return format!("+{p:.1}W");
        }
    }
    if status == "Discharging" {
        if let Some(p) = power {
            return format!("-{p:.1}W");
        }
    }
    if status == "Full" {
        return "100%".to_string();
    }
    if let Some(c) = capacity {
        return format!("{:.0}%", c.round());
    }
    String::new()
}

/// RGB color (0.0-1.0 per channel) associated with a battery status, used
/// by chart/indicator drawing code. Kept as a plain tuple (not egui::Color32)
/// so this crate stays UI-framework-agnostic.
pub fn color_for_status(status: &str) -> (f64, f64, f64) {
    match status {
        "Charging" => (0.18, 0.80, 0.44),
        "Discharging" => (0.90, 0.22, 0.38),
        "Full" => (0.22, 0.76, 0.62),
        _ => (0.60, 0.60, 0.60),
    }
}

/// Split `points` into contiguous runs wherever the time between two
/// consecutive samples (given by the parallel `ts_list`) exceeds max_gap
/// seconds -- e.g. the laptop was off, or the app wasn't running. Used so
/// a chart doesn't draw a connecting line straight across a stretch with
/// no actual readings.
pub fn split_on_gaps<T: Clone>(ts_list: &[f64], points: &[T], max_gap: f64) -> Vec<Vec<T>> {
    let mut chunks = Vec::new();
    let mut current: Vec<T> = Vec::new();
    for (i, pt) in points.iter().enumerate() {
        if !current.is_empty() && i < ts_list.len() && (ts_list[i] - ts_list[i - 1]) > max_gap {
            chunks.push(std::mem::take(&mut current));
        }
        current.push(pt.clone());
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    chunks
}

#[derive(Debug, Clone, PartialEq)]
pub struct IntervalBin {
    pub capacity: f64,
    pub status: String,
    pub timestamp: i64,
}

/// Merge adjacent interval bins together so we never draw more bars than
/// the canvas can meaningfully show (avoids sub-pixel bars / visual mush,
/// e.g. the "1m" x 24h = 1440-bin case).
pub fn aggregate_bins(data: &[IntervalBin], max_bins: usize) -> Vec<IntervalBin> {
    let n = data.len();
    if n <= max_bins || max_bins == 0 {
        return data.to_vec();
    }

    let step = n as f64 / max_bins as f64;
    let mut out = Vec::with_capacity(max_bins);
    for i in 0..max_bins {
        let a = (i as f64 * step) as usize;
        let b = (((i + 1) as f64 * step) as usize).max(a + 1).min(n);
        let chunk = &data[a..b];
        if chunk.is_empty() {
            continue;
        }
        let caps: Vec<f64> = chunk.iter().map(|x| x.capacity).collect();
        let chg = chunk.iter().filter(|x| x.status == "Charging").count();
        let dis = chunk.iter().filter(|x| x.status == "Discharging").count();
        let dom = if chg > dis {
            "Charging".to_string()
        } else if dis > chg {
            "Discharging".to_string()
        } else {
            chunk.last().unwrap().status.clone()
        };
        out.push(IntervalBin {
            capacity: caps.iter().sum::<f64>() / caps.len() as f64,
            status: dom,
            timestamp: chunk.last().unwrap().timestamp,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_label_charging_shows_signed_wattage() {
        assert_eq!(format_tray_label("Charging", Some(15.23), Some(60.0)), "+15.2W");
    }

    #[test]
    fn tray_label_discharging_shows_signed_wattage() {
        assert_eq!(format_tray_label("Discharging", Some(8.4), Some(60.0)), "-8.4W");
    }

    #[test]
    fn tray_label_full_shows_100_percent() {
        assert_eq!(format_tray_label("Full", Some(0.05), Some(100.0)), "100%");
    }

    #[test]
    fn tray_label_not_charging_falls_back_to_capacity() {
        assert_eq!(format_tray_label("Not charging", Some(0.0), Some(80.0)), "80%");
    }

    #[test]
    fn tray_label_no_data_is_blank() {
        assert_eq!(format_tray_label("Unknown", None, None), "");
    }

    #[test]
    fn tray_label_charging_with_null_power_falls_back_to_capacity_not_a_crash() {
        assert_eq!(format_tray_label("Charging", None, Some(42.0)), "42%");
    }

    #[test]
    fn split_on_gaps_breaks_at_the_gap() {
        let pts = vec![(0, 0), (1, 1), (2, 2), (3, 3), (4, 4)];
        let ts = vec![0.0, 1.0, 2.0, 100.0, 101.0];
        let chunks = split_on_gaps(&ts, &pts, 10.0);
        assert_eq!(chunks, vec![vec![(0, 0), (1, 1), (2, 2)], vec![(3, 3), (4, 4)]]);
    }

    #[test]
    fn split_on_gaps_single_chunk_without_a_gap() {
        let pts = vec![(0, 0), (1, 1), (2, 2)];
        let ts = vec![0.0, 1.0, 2.0];
        let chunks = split_on_gaps(&ts, &pts, 10.0);
        assert_eq!(chunks, vec![pts]);
    }

    #[test]
    fn split_on_gaps_empty_input() {
        let chunks: Vec<Vec<(i32, i32)>> = split_on_gaps(&[], &[], 10.0);
        assert!(chunks.is_empty());
    }

    #[test]
    fn aggregate_bins_merges_down_to_max_bins() {
        let data: Vec<IntervalBin> = (0..100)
            .map(|i| IntervalBin {
                capacity: i as f64,
                status: "Discharging".to_string(),
                timestamp: i,
            })
            .collect();
        let out = aggregate_bins(&data, 10);
        assert_eq!(out.len(), 10);
    }

    #[test]
    fn aggregate_bins_noop_when_already_small() {
        let data = vec![IntervalBin {
            capacity: 1.0,
            status: "Charging".to_string(),
            timestamp: 0,
        }];
        assert_eq!(aggregate_bins(&data, 10), data);
    }

    #[test]
    fn color_for_status_known_states_are_distinct() {
        use std::collections::HashSet;
        let statuses = ["Charging", "Discharging", "Full", "Not charging", "Unknown"];
        let colors: HashSet<_> = statuses
            .iter()
            .map(|s| {
                let c = color_for_status(s);
                (c.0.to_bits(), c.1.to_bits(), c.2.to_bits())
            })
            .collect();
        // "Not charging"/"Unknown" share the neutral fallback -> 4 distinct colors.
        assert_eq!(colors.len(), 4);
    }
}
