use std::fs;
use std::path::{Path, PathBuf};

use chrono::{Local, TimeZone};

use crate::history::HistoryDB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReportPeriod {
    Last24h,
    Last7d,
    Last30d,
}

impl ReportPeriod {
    pub fn label(&self) -> &'static str {
        match self {
            ReportPeriod::Last24h => "Last 24 hours",
            ReportPeriod::Last7d => "Last 7 days",
            ReportPeriod::Last30d => "Last 30 days",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "24h" => Some(ReportPeriod::Last24h),
            "7d" => Some(ReportPeriod::Last7d),
            "30d" => Some(ReportPeriod::Last30d),
            _ => None,
        }
    }

    fn seconds(&self) -> i64 {
        match self {
            ReportPeriod::Last24h => 24 * 3600,
            ReportPeriod::Last7d => 7 * 24 * 3600,
            ReportPeriod::Last30d => 30 * 24 * 3600,
        }
    }

    fn slug(&self) -> &'static str {
        match self {
            ReportPeriod::Last24h => "24h",
            ReportPeriod::Last7d => "7d",
            ReportPeriod::Last30d => "30d",
        }
    }
}

pub fn reports_dir() -> PathBuf {
    crate::history::default_db_path().parent().map(|p| p.join("reports")).unwrap_or_else(|| PathBuf::from("reports"))
}

#[derive(Debug, Clone)]
pub struct ReportEntry {
    pub name: String,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified: Option<chrono::DateTime<Local>>,
}

/// Generates a CSV export of real recorded history for `period` and
/// writes it under `reports_dir()`. Only CSV is implemented -- the design
/// reference's "format" selector also implied PDF, but this app has no
/// PDF-generation dependency and won't fake one.
///
/// Not a scheduled/emailed report -- see ROADMAP.md for why that stayed
/// an honest omission (no real SMTP/scheduling mechanism exists here).
pub fn generate_csv_report(db: &HistoryDB, period: ReportPeriod) -> Result<PathBuf, String> {
    let data = db.get_graph_data(period.seconds(), 2000);

    let dir = reports_dir();
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    let filename = format!("battery-report-{}-{}.csv", period.slug(), Local::now().format("%Y%m%d-%H%M%S"));
    let path = dir.join(filename);

    let mut csv = String::from("timestamp,status,capacity_percent,voltage_v,current_ma,power_w,temperature_c\n");
    for i in 0..data.timestamps.len() {
        let ts = Local
            .timestamp_opt(data.timestamps[i] as i64, 0)
            .single()
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%S").to_string())
            .unwrap_or_default();
        csv.push_str(&format!(
            "{},{},{},{},{},{},{}\n",
            ts,
            data.status.get(i).cloned().unwrap_or_default(),
            data.capacity.get(i).copied().unwrap_or_default(),
            data.voltage.get(i).copied().unwrap_or_default(),
            data.current_ma.get(i).copied().unwrap_or_default(),
            data.power.get(i).copied().unwrap_or_default(),
            data.temperature.get(i).copied().unwrap_or_default(),
        ));
    }

    fs::write(&path, csv).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Previously generated reports, newest first -- read straight off disk,
/// not tracked separately, so this is always in sync with what's
/// actually there.
pub fn list_reports() -> Vec<ReportEntry> {
    let dir = reports_dir();
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut reports: Vec<ReportEntry> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("csv") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            let modified = metadata.modified().ok().map(chrono::DateTime::<Local>::from);
            Some(ReportEntry {
                name: path.file_name()?.to_string_lossy().to_string(),
                path,
                size_bytes: metadata.len(),
                modified,
            })
        })
        .collect();
    reports.sort_by(|a, b| b.modified.cmp(&a.modified));
    reports
}

/// Refuses to delete anything outside `reports_dir()` -- `path` comes
/// straight from the frontend (echoed back from `get_reports()` in the
/// normal flow, but nothing on the Rust side enforced that), so without
/// this check a bug or a compromised webview could turn this into an
/// arbitrary-file-delete primitive.
pub fn delete_report(path: &Path) -> Result<(), String> {
    let dir = fs::canonicalize(reports_dir()).map_err(|e| e.to_string())?;
    let target = fs::canonicalize(path).map_err(|e| e.to_string())?;
    if target.parent() != Some(dir.as_path()) {
        return Err("refusing to delete a path outside the reports directory".to_string());
    }
    fs::remove_file(&target).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_period_round_trips_through_str() {
        for p in [ReportPeriod::Last24h, ReportPeriod::Last7d, ReportPeriod::Last30d] {
            assert_eq!(ReportPeriod::from_str(p.slug()), Some(p));
        }
        assert_eq!(ReportPeriod::from_str("bogus"), None);
    }

    #[test]
    fn generate_csv_report_writes_a_header_even_with_no_history() {
        // reports_dir() is derived from the real default_db_path() (not
        // parameterized), so this writes to the same on-disk location the
        // running app would use -- cleaned up immediately after, same as
        // `list_reports_ignores_non_csv_files` below.
        let db = HistoryDB::open_in_memory();
        let path = generate_csv_report(&db, ReportPeriod::Last24h).unwrap();
        let contents = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).ok();

        assert_eq!(contents.trim(), "timestamp,status,capacity_percent,voltage_v,current_ma,power_w,temperature_c");
    }

    #[test]
    fn delete_report_refuses_a_path_outside_reports_dir() {
        let dir = reports_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let outside = std::env::temp_dir().join(format!("battery-core-reports-test-outside-{}.txt", std::process::id()));
        std::fs::write(&outside, "not a report").unwrap();

        let result = delete_report(&outside);

        let still_there = outside.exists();
        std::fs::remove_file(&outside).ok();

        assert!(result.is_err());
        assert!(still_there); // must not have been deleted
    }

    #[test]
    fn list_reports_ignores_non_csv_files() {
        let dir = reports_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let stray = dir.join("not-a-report.txt");
        std::fs::write(&stray, "hello").unwrap();
        let before = list_reports().len();
        std::fs::remove_file(&stray).unwrap();
        let after = list_reports().len();
        assert_eq!(before, after); // the .txt file was never counted
    }
}
