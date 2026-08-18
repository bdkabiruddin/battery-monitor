use crate::format::IntervalBin;
use crate::reader::Reading;
use chrono::{Datelike, Duration, Local, NaiveDateTime};
use rusqlite::{params, Connection};
use std::collections::HashMap;
use std::path::PathBuf;

/// Default location for the history database: `$XDG_DATA_HOME/battery-monitor/battery_history.db`,
/// falling back to `~/.local/share/battery-monitor/battery_history.db`.
pub fn default_db_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("battery-monitor").join("battery_history.db")
}

const TS_FMT: &str = "%Y-%m-%dT%H:%M:%S";

/// Default history retention -- overridden by the user-configurable
/// setting (Settings screen), this is only the fallback used for the
/// safety-net prune that runs at DB-open time, before any setting has
/// been read.
pub const DEFAULT_RETENTION_DAYS: i64 = 35;

/// All public methods are defensive: sqlite I/O can fail (disk full,
/// locked file, corrupted db, permissions) and callers should never have
/// that take down the poll loop that drives the whole UI.
pub struct HistoryDB {
    conn: Option<Connection>,
}

#[derive(Debug, Clone, Default)]
pub struct GraphData {
    pub timestamps: Vec<f64>,
    pub voltage: Vec<f64>,
    pub current_ma: Vec<f64>,
    pub power: Vec<f64>,
    pub temperature: Vec<f64>,
    pub capacity: Vec<f64>,
    pub status: Vec<String>,
}

impl HistoryDB {
    pub fn open(path: &std::path::Path) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match Connection::open(path) {
            Ok(conn) => Self::init(conn),
            Err(exc) => {
                eprintln!("[battery-monitor] failed to open/initialize database: {exc}");
                HistoryDB { conn: None }
            }
        }
    }

    pub fn open_in_memory() -> Self {
        match Connection::open_in_memory() {
            Ok(conn) => Self::init(conn),
            Err(exc) => {
                eprintln!("[battery-monitor] failed to open/initialize database: {exc}");
                HistoryDB { conn: None }
            }
        }
    }

    fn init(conn: Connection) -> Self {
        let result = conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS readings (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    timestamp TEXT NOT NULL,
                    status TEXT NOT NULL,
                    capacity REAL,
                    voltage REAL,
                    current_a REAL,
                    power_w REAL,
                    temperature_c REAL,
                    charge_now_ah REAL
                );
                CREATE INDEX IF NOT EXISTS idx_time ON readings(timestamp);
                -- One row per calendar month, holding that month's latest-known
                -- health % -- deliberately a separate table from `readings`,
                -- which is pruned after 35 days (see `prune`). A real 12-month
                -- degradation trend needs data that outlives that window; this
                -- table does, at negligible size (one row/month).
                CREATE TABLE IF NOT EXISTS health_snapshots (
                    month TEXT PRIMARY KEY,
                    timestamp TEXT NOT NULL,
                    health_percent REAL NOT NULL
                );",
            );
        match result {
            Ok(()) => {
                let db = HistoryDB { conn: Some(conn) };
                db.prune(DEFAULT_RETENTION_DAYS);
                db
            }
            Err(exc) => {
                eprintln!("[battery-monitor] failed to open/initialize database: {exc}");
                HistoryDB { conn: None }
            }
        }
    }

    pub fn is_ok(&self) -> bool {
        self.conn.is_some()
    }

    /// Deletes readings older than `keep_days`. Callers own deciding when
    /// to call this -- at DB-open time with the default, and again
    /// whenever the user's configured retention changes or on a periodic
    /// timer, since a long-running process won't re-open the DB to pick
    /// up a new setting on its own.
    pub fn prune(&self, keep_days: i64) {
        let Some(conn) = &self.conn else { return };
        let cutoff = (Local::now() - Duration::days(keep_days)).format(TS_FMT).to_string();
        if let Err(exc) = conn.execute("DELETE FROM readings WHERE timestamp < ?1", params![cutoff]) {
            eprintln!("[battery-monitor] prune failed: {exc}");
        }
    }

    pub fn clear_all(&self) {
        let Some(conn) = &self.conn else { return };
        if let Err(exc) = conn.execute("DELETE FROM readings", []) {
            eprintln!("[battery-monitor] clear_all failed: {exc}");
        }
    }

    pub fn row_count(&self) -> i64 {
        let Some(conn) = &self.conn else { return 0 };
        conn.query_row("SELECT COUNT(*) FROM readings", [], |row| row.get(0)).unwrap_or(0)
    }

    pub fn save(&self, r: &Reading) {
        let Some(conn) = &self.conn else { return };
        let ts = Local::now().format(TS_FMT).to_string();
        let result = conn.execute(
            "INSERT INTO readings
             (timestamp, status, capacity, voltage, current_a, power_w, temperature_c, charge_now_ah)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                ts,
                r.status,
                r.capacity,
                r.voltage,
                r.current_a,
                r.power,
                r.temperature,
                r.charge_now,
            ],
        );
        if let Err(exc) = result {
            eprintln!("[battery-monitor] save failed: {exc}");
        }

        // Keep this calendar month's snapshot current -- last-known health %
        // as of "now", overwritten on every tick until the month ends and it
        // freezes as that month's final reading. No entry at all if health
        // can't be computed (e.g. no design capacity reported).
        if let Some(health) = r.health {
            let month = Local::now().format("%Y-%m").to_string();
            let result = conn.execute(
                "INSERT INTO health_snapshots (month, timestamp, health_percent)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(month) DO UPDATE SET timestamp = excluded.timestamp, health_percent = excluded.health_percent",
                params![month, ts, health],
            );
            if let Err(exc) = result {
                eprintln!("[battery-monitor] health snapshot save failed: {exc}");
            }
        }
    }

    /// Returns up to ~max_points (timestamp, voltage, current, power,
    /// temperature, capacity, status) samples covering the last `seconds`.
    ///
    /// For short ranges this is just the raw rows. For long ranges (7d,
    /// 1 month) at 1 reading/sec the raw row count can reach the millions --
    /// pulling all of that out just to average it down to max_points would
    /// mean growing memory/CPU cost the longer the app has been logging.
    /// Instead we bucket and average in SQL (bucket width =
    /// seconds/max_points) so at most ~max_points rows ever cross out of
    /// SQLite, regardless of how much history exists.
    ///
    /// Buckets are only emitted for time windows that actually have a
    /// reading in them (plain SQL GROUP BY semantics) -- so a stretch with
    /// no rows (laptop was off, or the app wasn't running) shows up as an
    /// absence, not a fabricated flat/interpolated point.
    pub fn get_graph_data(&self, seconds: i64, max_points: i64) -> GraphData {
        let Some(conn) = &self.conn else {
            return GraphData::default();
        };

        let cutoff = (Local::now() - Duration::seconds(seconds)).format(TS_FMT).to_string();

        let count: i64 = match conn.query_row(
            "SELECT COUNT(*) FROM readings WHERE timestamp >= ?1",
            params![cutoff],
            |row| row.get(0),
        ) {
            Ok(c) => c,
            Err(exc) => {
                eprintln!("[battery-monitor] get_graph_data failed: {exc}");
                return GraphData::default();
            }
        };

        type Row = (
            String,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            Option<f64>,
            String,
        );

        let fetch = || -> rusqlite::Result<Vec<Row>> {
            if count <= max_points {
                let mut stmt = conn.prepare(
                    "SELECT timestamp, voltage, current_a, power_w, temperature_c, capacity, status
                     FROM readings
                     WHERE timestamp >= ?1
                     ORDER BY timestamp ASC",
                )?;
                let rows = stmt.query_map(params![cutoff], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?;
                rows.collect()
            } else {
                let bucket_seconds = (seconds as f64 / max_points as f64).max(1.0);
                let mut stmt = conn.prepare(
                    "SELECT
                        MAX(timestamp),
                        AVG(voltage), AVG(current_a), AVG(power_w),
                        AVG(temperature_c), AVG(capacity),
                        status
                     FROM (
                         SELECT
                             CAST((CAST(strftime('%s', timestamp) AS INTEGER)
                                   - CAST(strftime('%s', ?1) AS INTEGER)) / ?2 AS INTEGER) AS bucket,
                             timestamp, voltage, current_a, power_w, temperature_c, capacity, status
                         FROM readings
                         WHERE timestamp >= ?1
                     )
                     GROUP BY bucket
                     ORDER BY bucket ASC",
                )?;
                let rows = stmt.query_map(params![cutoff, bucket_seconds], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?;
                rows.collect()
            }
        };
        let rows_result = fetch();

        let rows = match rows_result {
            Ok(r) => r,
            Err(exc) => {
                eprintln!("[battery-monitor] get_graph_data failed: {exc}");
                return GraphData::default();
            }
        };

        fn ffill(raw: &[Option<f64>], scale: f64) -> Vec<f64> {
            let mut out = Vec::with_capacity(raw.len());
            let mut last = 0.0;
            for v in raw {
                if let Some(v) = v {
                    last = v * scale;
                }
                out.push(last);
            }
            out
        }

        let voltage_raw: Vec<Option<f64>> = rows.iter().map(|r| r.1).collect();
        let current_raw: Vec<Option<f64>> = rows.iter().map(|r| r.2).collect();
        let power_raw: Vec<Option<f64>> = rows.iter().map(|r| r.3).collect();
        let temp_raw: Vec<Option<f64>> = rows.iter().map(|r| r.4).collect();
        let capacity_raw: Vec<Option<f64>> = rows.iter().map(|r| r.5).collect();

        let mut timestamps = Vec::with_capacity(rows.len());
        let mut last_ts = Local::now().timestamp() as f64;
        for r in &rows {
            if let Ok(dt) = NaiveDateTime::parse_from_str(&r.0, TS_FMT) {
                last_ts = dt.and_utc().timestamp() as f64;
            }
            // On a malformed timestamp: keep the last good one rather than
            // corrupting the whole series.
            timestamps.push(last_ts);
        }

        GraphData {
            timestamps,
            voltage: ffill(&voltage_raw, 1.0),
            current_ma: ffill(&current_raw, 1000.0),
            power: ffill(&power_raw, 1.0),
            temperature: ffill(&temp_raw, 1.0),
            capacity: ffill(&capacity_raw, 1.0),
            status: rows.into_iter().map(|r| r.6).collect(),
        }
    }

    /// Buckets the last 24h of readings into `interval_minutes`-wide bins
    /// (dominant status by vote count, average capacity), forward-filling
    /// bins with no readings at all so gaps don't show as a drop to 0% --
    /// but leaving a genuine 0% reading (dead battery) alone instead of
    /// overwriting it with a stale value. The row-level bucketing/averaging
    /// happens in SQL; only the small (<=1440-element) forward-fill pass
    /// runs here.
    pub fn get_interval_capacity(&self, interval_minutes: i64) -> Vec<IntervalBin> {
        let Some(conn) = &self.conn else { return Vec::new() };
        if interval_minutes <= 0 {
            return Vec::new();
        }

        let now = Local::now();
        let start_time = now - Duration::hours(24);
        let start_str = start_time.format(TS_FMT).to_string();
        let start_epoch = start_time.timestamp();
        let bucket_seconds = interval_minutes * 60;

        let total_minutes = 24 * 60;
        let num_bins = (total_minutes / interval_minutes) as usize;

        let mut bins: Vec<Option<(f64, String)>> = vec![None; num_bins];

        let mut query = |conn: &Connection| -> rusqlite::Result<()> {
            let mut stmt = conn.prepare(
                "SELECT bin, AVG(capacity), SUM(is_chg), SUM(is_dis), status
                 FROM (
                     SELECT
                         CAST((CAST(strftime('%s', timestamp) AS INTEGER)
                               - CAST(strftime('%s', ?1) AS INTEGER)) / ?2 AS INTEGER) AS bin,
                         timestamp, capacity, status,
                         CASE WHEN status = 'Charging' THEN 1 ELSE 0 END AS is_chg,
                         CASE WHEN status = 'Discharging' THEN 1 ELSE 0 END AS is_dis
                     FROM readings
                     WHERE timestamp >= ?1 AND capacity IS NOT NULL
                 )
                 GROUP BY bin
                 ORDER BY bin ASC",
            )?;
            let rows = stmt.query_map(params![start_str, bucket_seconds], |row| {
                let bin: i64 = row.get(0)?;
                let avg_cap: f64 = row.get(1)?;
                let chg: i64 = row.get(2)?;
                let dis: i64 = row.get(3)?;
                let last_status: String = row.get(4)?;
                Ok((bin, avg_cap, chg, dis, last_status))
            })?;
            for row in rows {
                let (bin, avg_cap, chg, dis, last_status) = row?;
                if bin < 0 || bin as usize >= num_bins {
                    continue;
                }
                let dom_status = if chg > dis {
                    "Charging".to_string()
                } else if dis > chg {
                    "Discharging".to_string()
                } else {
                    last_status
                };
                bins[bin as usize] = Some((avg_cap, dom_status));
            }
            Ok(())
        };

        if let Err(exc) = query(conn) {
            eprintln!("[battery-monitor] get_interval_capacity failed: {exc}");
            return Vec::new();
        }

        // Forward-fill (and backfill the leading gap) so a bin with no
        // readings at all doesn't show up as a drop to 0% -- but a genuine
        // 0% reading is left alone.
        let mut last_cap = bins.iter().find_map(|b| b.as_ref().map(|(c, _)| *c)).unwrap_or(0.0);
        let mut last_stat = bins
            .iter()
            .find_map(|b| b.as_ref().map(|(_, s)| s.clone()))
            .unwrap_or_else(|| "Unknown".to_string());

        let mut history = Vec::with_capacity(num_bins);
        for (i, bin) in bins.into_iter().enumerate() {
            let (capacity, status) = match bin {
                Some((c, s)) => {
                    last_cap = c;
                    last_stat = s.clone();
                    (c, s)
                }
                None => (last_cap, last_stat.clone()),
            };
            history.push(IntervalBin {
                capacity,
                status,
                timestamp: start_epoch + (i as i64) * bucket_seconds,
            });
        }
        history
    }

    /// Average power draw over the last `seconds`, or `None` if there are
    /// no readings in that window (a fresh install, or a machine that's
    /// been off) -- never fabricated as 0.
    pub fn avg_power_since(&self, seconds: i64) -> Option<f64> {
        let conn = self.conn.as_ref()?;
        let cutoff = (Local::now() - Duration::seconds(seconds)).format(TS_FMT).to_string();
        conn.query_row(
            "SELECT AVG(power_w) FROM readings WHERE timestamp >= ?1 AND power_w IS NOT NULL",
            params![cutoff],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    }

    /// Average power draw over the last `seconds`, counting only
    /// `Discharging` readings -- a real historical estimate of "what does
    /// this machine actually pull when it's not charging", usable as a
    /// stand-in for live system draw while plugged in (which sysfs has no
    /// way to measure directly here -- see `battery_core::rapl` for the
    /// one real, if root-gated and CPU-only, alternative). `None` if
    /// there's no discharging history in the window at all -- never
    /// fabricated as 0 or as "unknown, so just show the charging power".
    pub fn avg_discharge_power_since(&self, seconds: i64) -> Option<f64> {
        let conn = self.conn.as_ref()?;
        let cutoff = (Local::now() - Duration::seconds(seconds)).format(TS_FMT).to_string();
        conn.query_row(
            "SELECT AVG(power_w) FROM readings WHERE timestamp >= ?1 AND power_w IS NOT NULL AND status = 'Discharging'",
            params![cutoff],
            |row| row.get(0),
        )
        .ok()
        .flatten()
    }

    /// Average discharge power for each hour-of-day (0-23, local time),
    /// across all kept history (up to the 35-day prune window) -- the
    /// Usage Heatmap's "drain intensity by hour". Only `Discharging`
    /// readings count, so a laptop that's plugged in all afternoon
    /// doesn't drag that hour's intensity down. An hour with no
    /// discharging readings at all comes back `None` (never
    /// interpolated/forward-filled -- unlike the capacity bins, an empty
    /// hour here just means "not enough history yet", not a gap in an
    /// otherwise-continuous series).
    pub fn hourly_drain_intensity(&self) -> [Option<f64>; 24] {
        let mut out = [None; 24];
        let Some(conn) = &self.conn else { return out };

        let fetch = || -> rusqlite::Result<Vec<(i64, f64)>> {
            let mut stmt = conn.prepare(
                "SELECT CAST(strftime('%H', timestamp) AS INTEGER) AS hr, AVG(power_w)
                 FROM readings
                 WHERE status = 'Discharging' AND power_w IS NOT NULL
                 GROUP BY hr",
            )?;
            let rows = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
            rows.collect()
        };

        match fetch() {
            Ok(rows) => {
                for (hr, avg) in rows {
                    if (0..24).contains(&hr) {
                        out[hr as usize] = Some(avg);
                    }
                }
            }
            Err(exc) => eprintln!("[battery-monitor] hourly_drain_intensity failed: {exc}"),
        }
        out
    }

    /// The last `months` calendar months (oldest first, ending at the
    /// current month) paired with that month's health-snapshot reading --
    /// `None` for any month the app wasn't running in yet (real absence,
    /// not interpolated). Backed by `health_snapshots`, not `readings`, so
    /// it isn't subject to the 35-day prune.
    pub fn monthly_health_trend(&self, months: i64) -> Vec<(String, Option<f64>)> {
        let labels: Vec<String> = (0..months).rev().map(|ago| month_label(Local::now(), ago)).collect();
        let Some(conn) = &self.conn else {
            return labels.into_iter().map(|m| (m, None)).collect();
        };

        let fetch = || -> rusqlite::Result<HashMap<String, f64>> {
            let mut stmt = conn.prepare("SELECT month, health_percent FROM health_snapshots")?;
            let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?)))?;
            rows.collect()
        };

        let by_month = match fetch() {
            Ok(m) => m,
            Err(exc) => {
                eprintln!("[battery-monitor] monthly_health_trend failed: {exc}");
                HashMap::new()
            }
        };

        labels.into_iter().map(|m| { let v = by_month.get(&m).copied(); (m, v) }).collect()
    }
}

/// `months_ago` calendar months before `now`, as a "YYYY-MM" label.
/// Chrono has no calendar-month arithmetic (`Duration` is fixed-unit), so
/// this walks year/month by hand rather than approximating months as a
/// fixed number of days.
fn month_label(now: chrono::DateTime<Local>, months_ago: i64) -> String {
    let total = now.year() as i64 * 12 + (now.month() as i64 - 1) - months_ago;
    let year = total.div_euclid(12);
    let month = total.rem_euclid(12) + 1;
    format!("{year:04}-{month:02}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn insert(db: &HistoryDB, ts: &str, status: &str, capacity: Option<f64>, voltage: f64, current_a: f64, power_w: f64, temp: f64) {
        db.conn
            .as_ref()
            .unwrap()
            .execute(
                "INSERT INTO readings
                 (timestamp, status, capacity, voltage, current_a, power_w, temperature_c, charge_now_ah)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL)",
                params![ts, status, capacity, voltage, current_a, power_w, temp],
            )
            .unwrap();
    }

    fn ts_at(base: chrono::DateTime<Local>, minutes: i64) -> String {
        (base + Duration::minutes(minutes)).format(TS_FMT).to_string()
    }

    #[test]
    fn small_range_is_raw_passthrough() {
        let db = HistoryDB::open_in_memory();
        let base = Local::now() - Duration::minutes(5);
        for i in 0..5 {
            insert(&db, &ts_at(base, i), "Discharging", Some(50.0), 12.0 + i as f64 * 0.1, 0.5, 6.0, 25.0);
        }
        let data = db.get_graph_data(600, 720);
        assert_eq!(data.timestamps.len(), 5);
        assert_eq!(data.voltage, vec![12.0, 12.1, 12.2, 12.3, 12.4]);
    }

    #[test]
    fn null_reading_forward_fills_instead_of_zero_spike() {
        let db = HistoryDB::open_in_memory();
        let base = Local::now() - Duration::minutes(5);
        let readings = [Some(12.0), Some(12.1), None, Some(12.3), Some(12.4)];
        for (i, v) in readings.iter().enumerate() {
            insert(&db, &ts_at(base, i as i64), "Discharging", Some(50.0), v.unwrap_or(0.0), 0.5, 6.0, 25.0);
        }
        // Match the Python test: it inserts NULL voltage for index 2 --
        // reproduce that exactly via raw SQL since `insert`'s signature
        // takes a plain f64.
        db.conn
            .as_ref()
            .unwrap()
            .execute("UPDATE readings SET voltage = NULL WHERE timestamp = ?1", params![ts_at(base, 2)])
            .unwrap();

        let data = db.get_graph_data(600, 720);
        assert_eq!(data.voltage, vec![12.0, 12.1, 12.1, 12.3, 12.4]);
    }

    #[test]
    fn malformed_timestamp_does_not_corrupt_whole_batch() {
        let db = HistoryDB::open_in_memory();
        let base = Local::now() - Duration::minutes(5);
        for i in [0, 1] {
            insert(&db, &ts_at(base, i), "Discharging", Some(50.0), 12.0, 0.5, 6.0, 25.0);
        }
        db.conn
            .as_ref()
            .unwrap()
            .execute(
                "INSERT INTO readings (timestamp, status, capacity, voltage, current_a, power_w, temperature_c, charge_now_ah)
                 VALUES ('not-a-real-timestamp', 'Discharging', 50.0, 12.0, 0.5, 6.0, 25.0, NULL)",
                [],
            )
            .unwrap();
        for i in [3, 4] {
            insert(&db, &ts_at(base, i), "Discharging", Some(50.0), 12.0, 0.5, 6.0, 25.0);
        }

        let data = db.get_graph_data(600, 720);
        assert_eq!(data.timestamps.len(), 5);
        let unique: std::collections::HashSet<_> = data.timestamps.iter().map(|t| t.to_bits()).collect();
        assert!(unique.len() >= 4); // old-bug regression: all 5 collapsing to "now"
    }

    #[test]
    fn large_range_buckets_in_sql_and_bounds_output() {
        let db = HistoryDB::open_in_memory();
        let base = Local::now() - Duration::hours(2);
        let n_rows = 5000;
        let span = 2 * 3600;
        for i in 0..n_rows {
            let t = base + Duration::seconds((span * i) / n_rows);
            let status = if i < n_rows - 5 { "Charging" } else { "Full" };
            insert(&db, &t.format(TS_FMT).to_string(), status, Some((i % 100) as f64), 12.0, 0.5, 6.0, 25.0);
        }

        let data = db.get_graph_data(span, 100);
        assert!(data.timestamps.len() <= 102);
        assert!(data.timestamps.len() < (n_rows / 10) as usize);
        assert!((data.power[0] - 6.0).abs() < 0.01);
        assert_eq!(data.status.last().unwrap(), "Full");
    }

    #[test]
    fn offline_gap_produces_no_synthetic_points() {
        let db = HistoryDB::open_in_memory();
        let now = Local::now();
        let window_start = now - Duration::hours(4);
        for i in 0..200 {
            insert(&db, &(window_start + Duration::seconds(i * 9)).format(TS_FMT).to_string(), "Discharging", Some(80.0), 12.0, 0.5, 6.0, 25.0);
        }
        let resume = now - Duration::minutes(30);
        for i in 0..200 {
            insert(&db, &(resume + Duration::seconds(i * 9)).format(TS_FMT).to_string(), "Discharging", Some(40.0), 12.0, 0.5, 6.0, 25.0);
        }

        let data = db.get_graph_data(4 * 3600, 200);
        let gaps: Vec<f64> = (1..data.timestamps.len())
            .map(|i| data.timestamps[i] - data.timestamps[i - 1])
            .collect();
        assert!(gaps.iter().cloned().fold(0.0, f64::max) > 3.0 * 3600.0 - 300.0);
        let seen: std::collections::HashSet<_> = data.capacity.iter().map(|v| (v * 10.0).round() as i64).collect();
        assert!(seen.iter().all(|v| *v == 800 || *v == 400));
    }

    #[test]
    fn disabled_db_returns_empty_shape_without_raising() {
        let db = HistoryDB { conn: None };
        let data = db.get_graph_data(3600, 720);
        assert!(data.timestamps.is_empty());
        assert!(data.voltage.is_empty());
        db.save(&Reading {
            status: "Discharging".into(),
            capacity: Some(1.0),
            voltage: Some(1.0),
            design_voltage: None,
            current_a: Some(1.0),
            charging_current: 0.0,
            discharging_current: 1.0,
            power: Some(1.0),
            temperature: Some(1.0),
            charge_now: Some(1.0),
            charge_full: None,
            design_capacity: None,
            energy_now: None,
            energy_full: None,
            energy_design: None,
            health: None,
            health_status: "Unknown".into(),
            cycle: None,
            charge_limit: None,
            time_remaining_str: String::new(),
            short_time_str: String::new(),
            manufacturer: String::new(),
            model: String::new(),
            technology: String::new(),
        }); // must not panic
        db.prune(35); // must not panic
    }

    #[test]
    fn genuine_zero_capacity_is_not_treated_as_missing() {
        let db = HistoryDB::open_in_memory();
        let now_ref = Local::now();
        let start_ref = now_ref - Duration::hours(24);
        let insert_at = |minutes: i64, status: &str, capacity: f64| {
            insert(&db, &(start_ref + Duration::minutes(minutes)).format(TS_FMT).to_string(), status, Some(capacity), 12.0, 0.1, 1.0, 25.0);
        };
        insert_at(90, "Discharging", 50.0); // bin 1
        insert_at(210, "Discharging", 0.0); // bin 3: genuinely dead battery
        insert_at(330, "Charging", 75.0); // bin 5

        let history = db.get_interval_capacity(60);
        assert_eq!(history.len(), 24);
        assert_eq!(history[0].capacity, 50.0); // backfilled from first available
        assert_eq!(history[2].capacity, 50.0); // forward-filled gap
        assert_eq!(history[3].capacity, 0.0); // the actual bug: must stay 0, not backfilled
        assert_eq!(history[3].status, "Discharging");
        assert_eq!(history[4].capacity, 0.0); // forward-filled from the genuine zero
        assert_eq!(history[5].capacity, 75.0);
        assert_eq!(history[23].capacity, 75.0); // forward-filled to the end
    }

    #[test]
    fn all_empty_bins_default_to_zero() {
        let db = HistoryDB::open_in_memory();
        let history = db.get_interval_capacity(60);
        assert!(history.iter().all(|h| h.capacity == 0.0));
        assert!(history.iter().all(|h| h.status == "Unknown"));
    }

    #[test]
    fn avg_power_since_averages_recent_readings_only() {
        let db = HistoryDB::open_in_memory();
        let now = Local::now();
        insert(&db, &(now - Duration::minutes(3)).format(TS_FMT).to_string(), "Discharging", Some(50.0), 12.0, 1.0, 10.0, 25.0);
        insert(&db, &(now - Duration::minutes(2)).format(TS_FMT).to_string(), "Discharging", Some(50.0), 12.0, 1.0, 20.0, 25.0);
        // Outside the 5-minute window -- must not affect the average.
        insert(&db, &(now - Duration::hours(2)).format(TS_FMT).to_string(), "Discharging", Some(50.0), 12.0, 1.0, 1000.0, 25.0);

        let avg = db.avg_power_since(300).unwrap();
        assert!((avg - 15.0).abs() < 0.01);
    }

    #[test]
    fn avg_power_since_returns_none_without_data() {
        let db = HistoryDB::open_in_memory();
        assert_eq!(db.avg_power_since(300), None);
    }

    #[test]
    fn avg_discharge_power_since_only_counts_discharging_readings() {
        let db = HistoryDB::open_in_memory();
        let now = Local::now();
        insert(&db, &(now - Duration::minutes(3)).format(TS_FMT).to_string(), "Discharging", Some(50.0), 12.0, 1.0, 10.0, 25.0);
        insert(&db, &(now - Duration::minutes(2)).format(TS_FMT).to_string(), "Discharging", Some(50.0), 12.0, 1.0, 20.0, 25.0);
        // Charging readings in the same window must not pull the average
        // toward a very different (charging) power figure.
        insert(&db, &(now - Duration::minutes(1)).format(TS_FMT).to_string(), "Charging", Some(60.0), 12.0, 1.0, 45.0, 25.0);

        let avg = db.avg_discharge_power_since(300).unwrap();
        assert!((avg - 15.0).abs() < 0.01);
    }

    #[test]
    fn avg_discharge_power_since_returns_none_when_only_charging_history_exists() {
        let db = HistoryDB::open_in_memory();
        let now = Local::now();
        insert(&db, &now.format(TS_FMT).to_string(), "Charging", Some(60.0), 12.0, 1.0, 45.0, 25.0);
        assert_eq!(db.avg_discharge_power_since(300), None);
    }

    #[test]
    fn hourly_drain_intensity_only_counts_discharging_and_leaves_other_hours_none() {
        let db = HistoryDB::open_in_memory();
        // Fixed timestamps so the hour-of-day is deterministic regardless
        // of when this test runs.
        insert(&db, "2024-01-01T05:00:00", "Discharging", Some(80.0), 12.0, 1.0, 8.0, 25.0);
        insert(&db, "2024-01-01T05:30:00", "Discharging", Some(78.0), 12.0, 1.0, 12.0, 25.0);
        // A charging reading in a different hour must not count.
        insert(&db, "2024-01-01T10:00:00", "Charging", Some(90.0), 12.0, 1.0, 30.0, 25.0);

        let hourly = db.hourly_drain_intensity();
        assert!((hourly[5].unwrap() - 10.0).abs() < 0.01);
        assert_eq!(hourly[10], None);
        assert_eq!(hourly[0], None);
    }

    #[test]
    fn hourly_drain_intensity_all_none_without_data() {
        let db = HistoryDB::open_in_memory();
        assert!(db.hourly_drain_intensity().iter().all(|h| h.is_none()));
    }

    #[test]
    fn month_label_walks_the_year_boundary() {
        let now = Local.with_ymd_and_hms(2026, 1, 15, 12, 0, 0).unwrap();
        assert_eq!(month_label(now, 0), "2026-01");
        assert_eq!(month_label(now, 1), "2025-12");
        assert_eq!(month_label(now, 13), "2024-12");
    }

    #[test]
    fn monthly_health_trend_only_has_the_current_month_after_one_save() {
        let db = HistoryDB::open_in_memory();
        db.save(&Reading { health: Some(87.5), ..Reading::default() });

        let trend = db.monthly_health_trend(3);
        assert_eq!(trend.len(), 3);
        let this_month = Local::now().format("%Y-%m").to_string();
        assert_eq!(trend.last().unwrap().0, this_month);
        assert_eq!(trend.last().unwrap().1, Some(87.5));
        // The two earlier months: no snapshot yet, real absence.
        assert_eq!(trend[0].1, None);
        assert_eq!(trend[1].1, None);
    }

    #[test]
    fn monthly_health_trend_upserts_within_the_same_month() {
        let db = HistoryDB::open_in_memory();
        db.save(&Reading { health: Some(80.0), ..Reading::default() });
        db.save(&Reading { health: Some(79.0), ..Reading::default() });

        let this_month = Local::now().format("%Y-%m").to_string();
        let trend = db.monthly_health_trend(1);
        assert_eq!(trend, vec![(this_month, Some(79.0))]);
    }

    #[test]
    fn monthly_health_trend_skips_readings_without_a_health_figure() {
        let db = HistoryDB::open_in_memory();
        db.save(&Reading { health: None, ..Reading::default() });

        let this_month = Local::now().format("%Y-%m").to_string();
        let trend = db.monthly_health_trend(1);
        assert_eq!(trend, vec![(this_month, None)]);
    }
}
