use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use battery_core::{
    autostart, battery_saver, charging, default_db_path, delete_report, find_battery_paths, generate_csv_report,
    list_reports, power_profile, rapl_available, read_active_usb_pd, read_package_power_watts,
    read_peripheral_devices, read_top_processes_by_cpu, run_powertop_scan as core_run_powertop_scan,
    BatteryAlertDetector, BatteryReader, CpuProcess, GraphData, HistoryDB, IntervalBin, PowerTopProcess, Reading,
    ReportPeriod, Severity, DEFAULT_RETENTION_DAYS,
};
use chrono::Local;
use serde::{Deserialize, Serialize};
use tauri::menu::{Menu, MenuItem};
use tauri::tray::{TrayIcon, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};
use tauri_plugin_notification::NotificationExt;

const UPDATE_MS: u64 = 1000;
const ALERTS_KEPT: usize = 200;
const DEFAULT_LOW_THRESHOLD: f64 = 20.0;
const DEFAULT_CRITICAL_THRESHOLD: f64 = 5.0;

#[derive(Clone)]
struct AlertItem {
    id: u64,
    severity: Severity,
    message: String,
    time: String,
    read: bool,
}

/// A panic while holding one of these locks (nowhere currently panics
/// while holding one, but the background poll thread is unsupervised — if
/// that ever changes) would otherwise poison the mutex permanently, making
/// every command touching shared state panic forever until the process
/// restarts. Recovering the guard instead just carries on with whatever
/// state existed at the panic — acceptable for in-memory UI-refresh data
/// that gets overwritten by the next 1s poll tick anyway.
trait LockIgnorePoison<T> {
    fn lock_ip(&self) -> MutexGuard<'_, T>;
}
impl<T> LockIgnorePoison<T> for Mutex<T> {
    fn lock_ip(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
struct Shared {
    reading: Reading,
    last_updated: String,
    alerts: Vec<AlertItem>,
    next_alert_id: u64,
}

struct DashboardState {
    shared: Arc<Mutex<Shared>>,
    // Separate connection from the background writer's -- WAL mode (set in
    // HistoryDB::open) lets both coexist without lock contention, same
    // pattern as the egui build's `AppState::history_reader`.
    history_reader: Mutex<HistoryDB>,
    battery_path: Option<PathBuf>,
    // Every internal battery, not just the primary one -- used to keep a
    // second internal battery (dual-battery laptops) out of the
    // peripherals list on the Devices page. It still won't contribute to
    // health/dashboard numbers (those track `battery_path` only), but at
    // least it isn't shown mislabeled as a wireless mouse/keyboard.
    battery_paths: Vec<PathBuf>,
    // Session-only thresholds the alert detector fires against -- seeded
    // from UPower's real configured Low/Critical percentages at startup,
    // adjustable from the Alerts screen, but not persisted to disk (yet).
    // Shared with the poll thread's detector via the same Mutex.
    alert_detector: Arc<Mutex<BatteryAlertDetector>>,
    // Source of truth for what `alert_detector` is currently set to --
    // the detector itself only consumes thresholds, it doesn't expose
    // them back out, so this is what `get_alert_thresholds` reads.
    alert_thresholds: Mutex<(f64, f64)>,
    // Serializes settings.json read-modify-write cycles. Without this, two
    // rapid patches (e.g. flipping a toggle right after dragging a slider)
    // can each read the same pre-change snapshot and the second write
    // silently discards the first's change -- both `save_settings` and
    // `save_settings_patch` take this before touching the file.
    settings_lock: Mutex<()>,
}

/// Holds the live tray icon, if the "Show in system tray" setting is
/// currently on -- `None` when it's off. Toggling the setting at runtime
/// creates/destroys this rather than just changing a flag, since Tauri's
/// tray icon is a real OS resource, not a CSS class. Directly mirrors
/// network-monitor's identical `TrayState`.
struct TrayState(Mutex<Option<TrayIcon>>);

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DashboardSnapshot {
    status: String,
    capacity_percent: Option<f64>,
    voltage: String,
    design_voltage: String,
    current_a: String,
    power_w: String,
    temperature_c: String,
    health_percent: String,
    health_status: String,
    cycle_count: String,
    charge_limit: String,
    design_capacity_wh: String,
    full_capacity_wh: String,
    current_capacity_wh: String,
    design_capacity_ah: String,
    full_capacity_ah: String,
    current_capacity_ah: String,
    time_remaining: String,
    manufacturer: String,
    model: String,
    technology: String,
    last_updated: String,
    has_battery: bool,
}

fn fmt_opt(v: Option<f64>, unit: &str, decimals: usize) -> String {
    match v {
        Some(v) => format!("{:.*}{}", decimals, v, unit),
        None => "—".to_string(),
    }
}

fn to_snapshot(r: &Reading, last_updated: &str) -> DashboardSnapshot {
    DashboardSnapshot {
        status: r.status.clone(),
        capacity_percent: r.capacity,
        voltage: fmt_opt(r.voltage, " V", 2),
        design_voltage: fmt_opt(r.design_voltage, " V", 2),
        current_a: fmt_opt(r.current_a, " A", 2),
        power_w: fmt_opt(r.power, " W", 1),
        temperature_c: fmt_opt(r.temperature, "°C", 1),
        health_percent: fmt_opt(r.health, "%", 0),
        health_status: r.health_status.clone(),
        cycle_count: r.cycle.map(|c| format!("{c:.0}")).unwrap_or_else(|| "—".to_string()),
        charge_limit: r.charge_limit.map(|c| format!("{c:.0}%")).unwrap_or_else(|| "Not set".to_string()),
        design_capacity_wh: fmt_opt(r.energy_design, " Wh", 1),
        full_capacity_wh: fmt_opt(r.energy_full, " Wh", 1),
        current_capacity_wh: fmt_opt(r.energy_now, " Wh", 1),
        design_capacity_ah: fmt_opt(r.design_capacity, " Ah", 2),
        full_capacity_ah: fmt_opt(r.charge_full, " Ah", 2),
        current_capacity_ah: fmt_opt(r.charge_now, " Ah", 2),
        time_remaining: r.time_remaining_str.clone(),
        manufacturer: r.manufacturer.clone(),
        model: r.model.clone(),
        technology: r.technology.clone(),
        last_updated: last_updated.to_string(),
        has_battery: r.status != "No Battery",
    }
}

#[tauri::command]
fn get_dashboard(state: tauri::State<DashboardState>) -> DashboardSnapshot {
    let s = state.shared.lock_ip();
    to_snapshot(&s.reading, &s.last_updated)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GraphDataDto {
    timestamps: Vec<f64>,
    voltage: Vec<f64>,
    current_ma: Vec<f64>,
    power: Vec<f64>,
    temperature: Vec<f64>,
    capacity: Vec<f64>,
    status: Vec<String>,
}

impl From<GraphData> for GraphDataDto {
    fn from(g: GraphData) -> Self {
        GraphDataDto {
            timestamps: g.timestamps,
            voltage: g.voltage,
            current_ma: g.current_ma,
            power: g.power,
            temperature: g.temperature,
            capacity: g.capacity,
            status: g.status,
        }
    }
}

#[tauri::command]
fn get_graph_data(state: tauri::State<DashboardState>, seconds: i64, max_points: i64) -> GraphDataDto {
    state.history_reader.lock_ip().get_graph_data(seconds, max_points).into()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct IntervalBinDto {
    capacity: f64,
    status: String,
    timestamp: i64,
}

impl From<IntervalBin> for IntervalBinDto {
    fn from(b: IntervalBin) -> Self {
        IntervalBinDto { capacity: b.capacity, status: b.status, timestamp: b.timestamp }
    }
}

#[tauri::command]
fn get_interval_capacity(state: tauri::State<DashboardState>, interval_minutes: i64) -> Vec<IntervalBinDto> {
    state
        .history_reader
        .lock_ip()
        .get_interval_capacity(interval_minutes)
        .into_iter()
        .map(Into::into)
        .collect()
}

#[tauri::command]
fn get_avg_power_24h(state: tauri::State<DashboardState>) -> String {
    match state.history_reader.lock_ip().avg_power_since(24 * 3600) {
        Some(w) => format!("{w:.1} W"),
        None => "—".to_string(),
    }
}

#[tauri::command]
fn get_hourly_drain_intensity(state: tauri::State<DashboardState>) -> Vec<Option<f64>> {
    state.history_reader.lock_ip().hourly_drain_intensity().to_vec()
}

// A week, not 24h -- a real discharge sample in the last 24h isn't
// guaranteed (a laptop can stay plugged in for days), and the window
// only has to be "recent enough to be representative", not tight.
const ESTIMATED_DRAW_WINDOW_SECS: i64 = 7 * 24 * 3600;

/// A real historical estimate of system draw (averaged over real
/// `Discharging` readings from the last 7 days), used as a stand-in for
/// live system power while charging -- see
/// `battery_core::history::avg_discharge_power_since` for why there's no
/// way to measure that live on most hardware. `None` (rendered as "—")
/// if there's no discharge history in the window at all yet, not
/// fabricated as 0 or as the charging power.
#[tauri::command]
fn get_estimated_system_draw(state: tauri::State<DashboardState>) -> String {
    match state.history_reader.lock_ip().avg_discharge_power_since(ESTIMATED_DRAW_WINDOW_SECS) {
        Some(w) => format!("{w:.1} W"),
        None => "—".to_string(),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct MonthlyHealthDto {
    month: String,
    health_percent: Option<f64>,
}

#[tauri::command]
fn get_monthly_health_trend(state: tauri::State<DashboardState>, months: i64) -> Vec<MonthlyHealthDto> {
    state
        .history_reader
        .lock_ip()
        .monthly_health_trend(months)
        .into_iter()
        .map(|(month, health_percent)| MonthlyHealthDto { month, health_percent })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CpuProcessDto {
    pid: String,
    name: String,
    cpu_percent: String,
    mem_percent: String,
    runtime: String,
}

impl From<CpuProcess> for CpuProcessDto {
    fn from(p: CpuProcess) -> Self {
        CpuProcessDto { pid: p.pid, name: p.name, cpu_percent: p.cpu_percent, mem_percent: p.mem_percent, runtime: p.runtime }
    }
}

#[tauri::command]
fn get_top_processes(limit: usize) -> Vec<CpuProcessDto> {
    read_top_processes_by_cpu(limit).into_iter().map(Into::into).collect()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PowerTopProcessDto {
    pid: Option<String>,
    name: String,
    impact: String,
    category: String,
}

impl From<PowerTopProcess> for PowerTopProcessDto {
    fn from(p: PowerTopProcess) -> Self {
        PowerTopProcessDto { pid: p.pid, name: p.name, impact: p.impact, category: p.category }
    }
}

/// pkexec-gated: prompts for a password. Only ever invoked from an
/// explicit user action (a "Scan" button), never from the background poll
/// loop -- see battery_core::processes::run_powertop_scan.
#[tauri::command]
fn run_powertop_scan(seconds: u32, limit: usize) -> Result<Vec<PowerTopProcessDto>, String> {
    core_run_powertop_scan(seconds, limit).map(|rows| rows.into_iter().map(Into::into).collect())
}

#[tauri::command]
fn get_rapl_available() -> bool {
    rapl_available()
}

/// pkexec-gated: prompts for a password. CPU package power only (RAPL
/// has no visibility into the display, SSD, etc.) -- the frontend labels
/// it as such, not as "system power". Manual-only, same rule as the
/// PowerTop scan above.
#[tauri::command]
fn measure_package_power() -> Result<f64, String> {
    read_package_power_watts()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PowerProfileState {
    profiles: Vec<String>,
    active: Option<String>,
}

#[tauri::command]
fn get_power_profile_state() -> PowerProfileState {
    PowerProfileState { profiles: power_profile::list_profiles(), active: power_profile::active_profile() }
}

/// Session-level via power-profiles-daemon's own polkit policy -- no
/// pkexec prompt on a normal desktop session.
#[tauri::command]
fn set_power_profile(name: String) -> Result<(), String> {
    power_profile::set_profile(&name)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct BatterySaverState {
    enabled: Option<bool>,
    low_percent: Option<f64>,
    critical_percent: Option<f64>,
}

#[tauri::command]
fn get_battery_saver_state() -> BatterySaverState {
    let (low_percent, critical_percent) = battery_saver::upower_low_critical_thresholds();
    BatterySaverState { enabled: battery_saver::auto_battery_saver_enabled(), low_percent, critical_percent }
}

/// Per-user gsettings key -- no root/pkexec needed.
#[tauri::command]
fn set_battery_saver(enabled: bool) -> Result<(), String> {
    battery_saver::set_auto_battery_saver(enabled)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct UsbPdDto {
    usb_type: String,
    voltage_v: Option<f64>,
    current_a: Option<f64>,
    max_current_a: Option<f64>,
    watts: Option<f64>,
}

#[tauri::command]
fn get_usb_pd() -> Option<UsbPdDto> {
    read_active_usb_pd().map(|p| UsbPdDto {
        usb_type: p.usb_type,
        voltage_v: p.voltage_v,
        current_a: p.current_a,
        max_current_a: p.max_current_a,
        watts: p.watts,
    })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ChargeThresholdsDto {
    supported: bool,
    start: Option<f64>,
    end: Option<f64>,
}

#[tauri::command]
fn get_charge_thresholds(state: tauri::State<DashboardState>) -> ChargeThresholdsDto {
    let t = charging::read_charge_thresholds(state.battery_path.as_deref());
    ChargeThresholdsDto { supported: t.supported, start: t.start, end: t.end }
}

/// pkexec-gated: prompts for a password. Manual-only (an explicit
/// "Apply" click) -- see battery_core::charging::set_charge_thresholds
/// for why this couldn't be live-verified against real hardware here.
#[tauri::command]
fn set_charge_thresholds(state: tauri::State<DashboardState>, start: u8, end: u8) -> Result<(), String> {
    let Some(path) = state.battery_path.as_deref() else {
        return Err("no battery detected".to_string());
    };
    charging::set_charge_thresholds(path, start, end)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AlertDto {
    id: u64,
    severity: String,
    message: String,
    time: String,
    read: bool,
}

#[tauri::command]
fn get_alerts(state: tauri::State<DashboardState>) -> Vec<AlertDto> {
    state
        .shared
        .lock_ip()
        .alerts
        .iter()
        .map(|a| AlertDto {
            id: a.id,
            severity: a.severity.label().to_string(),
            message: a.message.clone(),
            time: a.time.clone(),
            read: a.read,
        })
        .collect()
}

#[tauri::command]
fn mark_alert_read(state: tauri::State<DashboardState>, id: u64) {
    let mut s = state.shared.lock_ip();
    if let Some(a) = s.alerts.iter_mut().find(|a| a.id == id) {
        a.read = true;
    }
}

#[tauri::command]
fn mark_all_alerts_read(state: tauri::State<DashboardState>) {
    let mut s = state.shared.lock_ip();
    for a in s.alerts.iter_mut() {
        a.read = true;
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AlertThresholds {
    low: f64,
    critical: f64,
}

#[tauri::command]
fn get_alert_thresholds(state: tauri::State<DashboardState>) -> AlertThresholds {
    let (low, critical) = *state.alert_thresholds.lock_ip();
    AlertThresholds { low, critical }
}

/// Session-only: adjusts the running detector's thresholds immediately,
/// but doesn't write anywhere -- restarting the app reverts to UPower's
/// real configured values. See `DashboardState::alert_detector`.
#[tauri::command]
fn set_alert_thresholds(state: tauri::State<DashboardState>, low: f64, critical: f64) -> Result<(), String> {
    if critical >= low {
        return Err("critical threshold must be less than low threshold".to_string());
    }
    state.alert_detector.lock_ip().set_thresholds(low, critical);
    *state.alert_thresholds.lock_ip() = (low, critical);
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DeviceRowDto {
    name: String,
    manufacturer: String,
    status: String,
    capacity_percent: Option<f64>,
}

/// The laptop's own battery (from the live-polled `Reading`, same source
/// as the Dashboard) plus any other `type=Battery` sysfs devices --
/// wireless mice/keyboards with their own fuel gauge. See
/// `battery_core::devices` for what this can and can't see (sysfs-only,
/// not a full UPower D-Bus enumeration).
#[tauri::command]
fn get_devices(state: tauri::State<DashboardState>) -> Vec<DeviceRowDto> {
    let mut out = Vec::new();
    {
        let s = state.shared.lock_ip();
        if s.reading.status != "No Battery" {
            out.push(DeviceRowDto {
                name: if s.reading.model != "Unknown" { s.reading.model.clone() } else { "Internal Battery".to_string() },
                manufacturer: s.reading.manufacturer.clone(),
                status: s.reading.status.clone(),
                capacity_percent: s.reading.capacity,
            });
        }
    }
    for d in read_peripheral_devices(&state.battery_paths) {
        out.push(DeviceRowDto {
            name: d.name,
            manufacturer: d.manufacturer.unwrap_or_else(|| "Unknown".to_string()),
            status: d.status,
            capacity_percent: d.capacity_percent,
        });
    }
    out
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ReportEntryDto {
    name: String,
    path: String,
    size_bytes: u64,
    modified: String,
}

#[tauri::command]
fn get_reports() -> Vec<ReportEntryDto> {
    list_reports()
        .into_iter()
        .map(|r| ReportEntryDto {
            name: r.name,
            path: r.path.display().to_string(),
            size_bytes: r.size_bytes,
            modified: r.modified.map(|m| m.format("%Y-%m-%d %H:%M:%S").to_string()).unwrap_or_default(),
        })
        .collect()
}

/// Builds a real CSV export of this machine's own recorded history --
/// see battery_core::reports for what it contains and why there's no PDF
/// option or scheduled/emailed variant.
#[tauri::command]
fn generate_report(state: tauri::State<DashboardState>, period: String) -> Result<ReportEntryDto, String> {
    let period = ReportPeriod::from_str(&period).ok_or_else(|| format!("unknown period: {period}"))?;
    let path = generate_csv_report(&state.history_reader.lock_ip(), period)?;
    let metadata = std::fs::metadata(&path).map_err(|e| e.to_string())?;
    Ok(ReportEntryDto {
        name: path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
        path: path.display().to_string(),
        size_bytes: metadata.len(),
        modified: Local::now().format("%Y-%m-%d %H:%M:%S").to_string(),
    })
}

#[tauri::command]
fn remove_report(path: String) -> Result<(), String> {
    delete_report(std::path::Path::new(&path))
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
struct AppSettings {
    capacity_unit: String, // "Wh" | "Ah" -- purely a display preference, not a data-source choice
    refresh_interval_secs: u32,
    notif_desktop: bool,
    notif_tray: bool,
    #[serde(default = "default_retention_days")]
    history_retention_days: i64,
    // Set true the first time the window is closed-to-tray, so the
    // one-time "still running in the tray" notification never repeats
    // after that -- not a per-session flag, a permanent one.
    #[serde(default)]
    has_shown_tray_hint: bool,
}

fn default_retention_days() -> i64 {
    DEFAULT_RETENTION_DAYS
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            capacity_unit: "Wh".to_string(),
            refresh_interval_secs: 1,
            notif_desktop: true,
            notif_tray: true,
            history_retention_days: DEFAULT_RETENTION_DAYS,
            has_shown_tray_hint: false,
        }
    }
}

fn settings_path() -> PathBuf {
    default_db_path().parent().map(|p| p.join("settings.json")).unwrap_or_else(|| PathBuf::from("settings.json"))
}

#[tauri::command]
fn get_settings() -> AppSettings {
    std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

/// Shared with the close-to-tray handler, which needs to persist the
/// `has_shown_tray_hint` flag without going through the `save_settings`
/// command (that needs `DashboardState` for its re-prune side effect,
/// which isn't relevant here).
fn write_settings_file(settings: &AppSettings) -> Result<(), String> {
    if let Some(parent) = settings_path().parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(settings).map_err(|e| e.to_string())?;
    std::fs::write(settings_path(), json).map_err(|e| e.to_string())
}

#[tauri::command]
fn save_settings(settings: AppSettings, state: tauri::State<DashboardState>) -> Result<(), String> {
    let _guard = state.settings_lock.lock_ip();
    write_settings_file(&settings)?;
    // Applied immediately rather than waiting for the next periodic prune
    // tick or app restart -- lowering the retention should take effect
    // right away, not silently next hour.
    state.history_reader.lock_ip().prune(settings.history_retention_days);
    Ok(())
}

/// Atomically applies a partial patch of settings fields: read, merge,
/// write all happen under `settings_lock` in one round trip, unlike the
/// old frontend pattern of a separate `get_settings` + `save_settings`
/// call, where two rapid patches could interleave and one would silently
/// discard the other. Returns the merged settings so the frontend doesn't
/// need a follow-up read.
#[tauri::command]
fn save_settings_patch(patch: serde_json::Value, state: tauri::State<DashboardState>) -> Result<AppSettings, String> {
    let _guard = state.settings_lock.lock_ip();
    let mut current = serde_json::to_value(get_settings()).map_err(|e| e.to_string())?;
    if let (Some(current_obj), Some(patch_obj)) = (current.as_object_mut(), patch.as_object()) {
        for (key, value) in patch_obj {
            current_obj.insert(key.clone(), value.clone());
        }
    }
    let merged: AppSettings = serde_json::from_value(current).map_err(|e| e.to_string())?;
    write_settings_file(&merged)?;
    state.history_reader.lock_ip().prune(merged.history_retention_days);
    Ok(merged)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HistoryStatsDto {
    db_path: String,
    row_count: i64,
}

#[tauri::command]
fn get_history_stats(state: tauri::State<DashboardState>) -> HistoryStatsDto {
    HistoryStatsDto {
        db_path: default_db_path().display().to_string(),
        row_count: state.history_reader.lock_ip().row_count(),
    }
}

/// Permanently deletes all recorded telemetry history. The frontend is
/// responsible for confirming with the user first -- this has no undo.
#[tauri::command]
fn clear_history(state: tauri::State<DashboardState>) {
    state.history_reader.lock_ip().clear_all();
}

#[tauri::command]
fn get_autostart() -> bool {
    autostart::is_enabled()
}

#[tauri::command]
fn set_autostart(enabled: bool) -> Result<(), String> {
    if enabled { autostart::enable() } else { autostart::disable() }
}

#[tauri::command]
fn get_app_version() -> String {
    env!("CARGO_PKG_VERSION").to_string()
}

/// Restores and focuses the existing window rather than doing nothing --
/// a second launch (double-clicking the app again, an autostart entry
/// firing while it's already running from a previous login) should feel
/// like "bring it to front", not silently no-op.
fn show_main_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("main") {
        // `show()` + `set_focus()` alone isn't enough on every Linux WM:
        // if the window was minimized (not just hidden-to-tray),
        // `show()` doesn't implicitly unminimize it on GNOME/some X11
        // WMs, and even when it does, the WM can leave it behind
        // whatever's currently focused instead of raising it. The
        // always-on-top toggle forces a raise on X11 WMs that respect
        // it; on Wayland compositors that ignore unsolicited raise/focus
        // requests entirely (by design, not a bug this app can work
        // around), the taskbar/dock entry still flashes as attention-
        // requesting.
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.set_always_on_top(true);
        let _ = w.set_always_on_top(false);
    }
}

fn build_tray(app: &AppHandle) -> tauri::Result<TrayIcon> {
    let show_item = MenuItem::with_id(app, "show", "Show Battery Monitor", true, None::<&str>)?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&show_item, &quit_item])?;

    let icon = app
        .default_window_icon()
        .cloned()
        .ok_or_else(|| tauri::Error::AssetNotFound("no default window icon configured in tauri.conf.json".to_string()))?;
    TrayIconBuilder::new()
        .icon(icon)
        .tooltip("Battery Monitor")
        // Real "<percent>% · <status>" text beside the icon -- set to a
        // real value within one poll tick (1s) by the background loop;
        // "—" here is an honest "no reading yet" placeholder, not a
        // fabricated number.
        .title("—")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id.as_ref() {
            "show" => show_main_window(app),
            "quit" => app.exit(0),
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: tauri::tray::MouseButton::Left,
                button_state: tauri::tray::MouseButtonState::Up,
                ..
            } = event
            {
                show_main_window(tray.app_handle());
            }
        })
        .build(app)
}

#[tauri::command]
fn set_tray_enabled(enabled: bool, app: AppHandle, tray_state: tauri::State<TrayState>) -> Result<(), String> {
    let mut slot = tray_state.0.lock_ip();
    if enabled {
        if slot.is_none() {
            *slot = Some(build_tray(&app).map_err(|e| e.to_string())?);
        }
    } else {
        *slot = None; // Dropping the TrayIcon removes it from the OS tray.
    }
    Ok(())
}

pub fn run() {
    tauri::Builder::default()
        // Must be the first plugin registered (Tauri's own recommendation)
        // so it can intercept a second launch before anything else in the
        // chain runs. Without this, a second launch spawns a second
        // background poll thread writing to the same SQLite database and
        // burning real CPU unnoticed -- exactly what happened during this
        // project's own testing (a stray instance ran for 2.5 hours before
        // being caught). The second instance's `main()` exits right after
        // this fires in the *first* instance.
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show_main_window(app);
        }))
        .plugin(tauri_plugin_notification::init())
        // Restores the window's real last size/position on launch instead
        // of always resetting to tauri.conf.json's 1300x900 default --
        // saved automatically on move/resize/close.
        .plugin(tauri_plugin_window_state::Builder::new().build())
        .setup(|app| {
            if cfg!(debug_assertions) {
                app.handle().plugin(
                    tauri_plugin_log::Builder::default()
                        .level(log::LevelFilter::Info)
                        .build(),
                )?;
            }

            let shared = Arc::new(Mutex::new(Shared::default()));
            let db_path = default_db_path();
            let battery_paths = find_battery_paths();
            let battery_path = battery_paths.first().cloned();

            // Seed the alert thresholds from UPower's real configured
            // Low/Critical percentages -- falls back to conservative
            // defaults only if UPower.conf can't be read at all.
            let (upower_low, upower_critical) = battery_saver::upower_low_critical_thresholds();
            let initial_low = upower_low.unwrap_or(DEFAULT_LOW_THRESHOLD);
            let initial_critical = upower_critical.unwrap_or(DEFAULT_CRITICAL_THRESHOLD);
            let alert_detector = Arc::new(Mutex::new(BatteryAlertDetector::new(initial_low, initial_critical)));

            {
                let shared = Arc::clone(&shared);
                let db_path = db_path.clone();
                let battery_path = battery_path.clone();
                let alert_detector = Arc::clone(&alert_detector);
                let app_handle = app.handle().clone();
                std::thread::spawn(move || {
                    let db = HistoryDB::open(&db_path);
                    let reader = BatteryReader::new(battery_path);
                    let mut tick: u64 = 0;
                    // Re-prune roughly hourly against the user's current
                    // setting -- `save_settings` already re-prunes
                    // immediately when the setting changes, but a
                    // long-running process (no restart, setting never
                    // touched) still needs *some* periodic enforcement, not
                    // just the one-shot prune at DB-open time.
                    let prune_every_ticks = (3_600_000 / UPDATE_MS).max(1);
                    loop {
                        // A panic inside one tick (e.g. an unexpected sysfs
                        // read failure) used to kill this thread silently --
                        // `Shared` would freeze at its last-good value
                        // forever with no error surfaced anywhere in the UI.
                        // Catching it here keeps the loop (and telemetry)
                        // alive instead.
                        let tick_result = catch_unwind(AssertUnwindSafe(|| {
                            let reading = reader.get();
                            db.save(&reading);
                            tick += 1;
                            if tick % prune_every_ticks == 0 {
                                db.prune(get_settings().history_retention_days);
                            }

                            // Live label beside the tray icon (AppIndicator/
                            // KStatusNotifierItem -- GNOME needs the AppIndicator
                            // extension, already required for the tray icon
                            // itself to render at all). Same signed-wattage
                            // format as the companion GNOME Shell extension this
                            // rewrite is replacing ("+15.2W" charging, "-8.4W"
                            // discharging, "100%"/"<capacity>%" otherwise) --
                            // see battery_core::format::format_tray_label.
                            // Skipped when the tray is off so this doesn't do
                            // pointless work every tick.
                            if let Some(tray_state) = app_handle.try_state::<TrayState>() {
                                if let Some(tray) = tray_state.0.lock_ip().as_ref() {
                                    let label = battery_core::format_tray_label(&reading.status, reading.power, reading.capacity);
                                    let _ = tray.set_title(Some(label));
                                }
                            }

                            let new_alerts = alert_detector.lock_ip().observe(&reading);
                            {
                                let mut s = shared.lock_ip();
                                s.reading = reading;
                                s.last_updated = Local::now().format("%H:%M:%S").to_string();
                                for (severity, message) in &new_alerts {
                                    let id = s.next_alert_id;
                                    s.next_alert_id += 1;
                                    s.alerts.insert(
                                        0,
                                        AlertItem {
                                            id,
                                            severity: *severity,
                                            message: message.clone(),
                                            time: Local::now().format("%H:%M:%S").to_string(),
                                            read: false,
                                        },
                                    );
                                }
                                s.alerts.truncate(ALERTS_KEPT);
                            }
                            // Real desktop notification, only if the user's
                            // preference (settings.json, same file the
                            // Settings screen reads/writes) has it on --
                            // reads it fresh each time rather than caching,
                            // so a toggle flip takes effect on the very next
                            // alert, not just after a restart.
                            if !new_alerts.is_empty() && get_settings().notif_desktop {
                                for (severity, message) in &new_alerts {
                                    let _ = app_handle
                                        .notification()
                                        .builder()
                                        .title(format!("Battery Monitor — {}", severity.label()))
                                        .body(message)
                                        .show();
                                }
                            }
                        }));
                        if let Err(e) = tick_result {
                            let msg = e
                                .downcast_ref::<&str>()
                                .map(|s| s.to_string())
                                .or_else(|| e.downcast_ref::<String>().cloned())
                                .unwrap_or_else(|| "unknown panic".to_string());
                            log::error!("battery poll tick panicked, recovering: {msg}");
                        }
                        std::thread::sleep(Duration::from_millis(UPDATE_MS));
                    }
                });
            }

            app.manage(DashboardState {
                shared,
                history_reader: Mutex::new(HistoryDB::open(&db_path)),
                battery_path,
                battery_paths,
                alert_detector,
                alert_thresholds: Mutex::new((initial_low, initial_critical)),
                settings_lock: Mutex::new(()),
            });

            // Tray icon starts up matching the persisted setting, not
            // unconditionally -- a user who turned it off shouldn't see
            // it reappear on every launch.
            let initial_tray = if get_settings().notif_tray { Some(build_tray(app.handle())?) } else { None };
            app.manage(TrayState(Mutex::new(initial_tray)));

            // Close button hides to tray when the tray icon exists, quits
            // normally when it doesn't -- checked fresh on every close so
            // toggling the setting takes effect immediately.
            if let Some(window) = app.get_webview_window("main") {
                let app_handle = app.handle().clone();
                window.on_window_event(move |event| {
                    if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                        let has_tray = app_handle.try_state::<TrayState>().map(|s| s.0.lock_ip().is_some()).unwrap_or(false);
                        if has_tray {
                            api.prevent_close();
                            if let Some(w) = app_handle.get_webview_window("main") {
                                let _ = w.hide();
                            }
                            // One-time only, ever -- a user closing the
                            // window repeatedly shouldn't get renagged
                            // every time; the flag persists in
                            // settings.json once shown.
                            let mut settings = get_settings();
                            if !settings.has_shown_tray_hint {
                                let _ = app_handle
                                    .notification()
                                    .builder()
                                    .title("Battery Monitor")
                                    .body("Still running in the system tray -- click the tray icon to reopen, or Quit from its menu to exit fully.")
                                    .show();
                                settings.has_shown_tray_hint = true;
                                let _ = write_settings_file(&settings);
                            }
                        }
                    }
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_dashboard,
            get_graph_data,
            get_interval_capacity,
            get_avg_power_24h,
            get_hourly_drain_intensity,
            get_estimated_system_draw,
            get_monthly_health_trend,
            get_top_processes,
            run_powertop_scan,
            get_rapl_available,
            measure_package_power,
            get_power_profile_state,
            set_power_profile,
            get_battery_saver_state,
            set_battery_saver,
            get_usb_pd,
            get_charge_thresholds,
            set_charge_thresholds,
            get_alerts,
            mark_alert_read,
            mark_all_alerts_read,
            get_alert_thresholds,
            set_alert_thresholds,
            get_devices,
            get_reports,
            generate_report,
            remove_report,
            get_settings,
            save_settings,
            save_settings_patch,
            get_history_stats,
            clear_history,
            get_autostart,
            set_autostart,
            get_app_version,
            set_tray_enabled,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
