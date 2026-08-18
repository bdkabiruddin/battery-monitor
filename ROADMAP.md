# Battery Monitor — feature status

A Tauri 2 desktop app: a Rust backend (`app/`, backed by the
`battery-core` library at `core/`) driving a plain HTML/CSS/JS frontend
(`frontend/`, no bundler or build step).

## Architecture

- `core/` — `battery-core`, a data-layer library with no UI code:
  `/sys/class/power_supply/BAT*/*`, `ps`, `pkexec powertop`,
  `power-profiles-daemon`, GNOME Settings Daemon/UPower, USB-C Power
  Delivery sysfs, Intel RAPL, and a SQLite history store.
- `app/` — Tauri commands and app state. A background thread polls the
  battery every 1s into shared state and writes history; everything else
  is a synchronous on-demand command.
- `frontend/` — static HTML/CSS/JS driving the above via Tauri's
  `invoke()`.

Single-instance enforced via `tauri-plugin-single-instance`; a second
launch attempt focuses the existing window instead of starting a second
process.

## Screens

- **Dashboard** — charge ring, live voltage/current/power/temperature
  with rolling sparklines, battery information, and a 24h capacity
  chart.
- **Power Usage** — process table ranked by CPU (`ps`-based, live) and a
  manual PowerTop scan (`pkexec`, 15s) for per-process power impact.
- **Live Telemetry** — dual capacity/power chart over a selectable
  range, session stats, and a 24-hour discharge-intensity heatmap.
- **Health** — health percentage, cycle count, and a 12-month
  degradation trend (one snapshot per calendar month). Calibration,
  charge-cycle log, and device comparison are not implemented — no
  trigger exists for the first, sysfs has no per-cycle log for the
  second, and there's no comparable-device dataset for the third.
- **Charging** — estimated system draw (7-day historical average, since
  no sysfs source reports live system draw while charging), CPU package
  power via Intel RAPL (`pkexec`-gated, manual), power profile
  (`power-profiles-daemon`), automatic battery saver status (GNOME
  Settings Daemon), USB-C Power Delivery, and charge threshold
  read/write where the hardware supports it. Wear-leveling scheduling is
  not implemented — no standard Linux facility exists for it.
- **Alerts** — an edge-triggered detector for low/critical battery,
  high temperature, and abnormal power-draw spikes, seeded from UPower's
  configured thresholds and adjustable per session.
- **History** — CSV export of recorded history for a selected period.
  Scheduled email reports are not implemented (no SMTP configuration
  collected).
- **Devices** — other `type=Battery` sysfs devices (wireless
  peripherals with their own fuel gauge). Devices reachable only via
  UPower's D-Bus interface, with no sysfs node, aren't discovered yet.
- **Settings** — capacity unit toggle, refresh interval, autostart,
  desktop notifications, history retention with automatic pruning, and
  history management (clear all, live row count). No i18n or
  update-check mechanism yet.

## System tray

Live icon with a wattage/percentage label (matching the sign convention
of charging/discharging), a Show/Quit menu, and close-to-tray behavior
gated by the "Show in system tray" setting.

## Known gaps / deliberately deferred

- Calibration, charge-cycle log, device comparison, wear-leveling
  scheduling — see Health/Charging above.
- Scheduled email reports — needs SMTP configuration nothing here
  collects.
- No i18n — the language setting isn't offered since there's no
  translation system.
- "Check for updates" — no update-check mechanism implemented yet.
- UPower D-Bus device enumeration (Devices screen) — currently sysfs-only.
