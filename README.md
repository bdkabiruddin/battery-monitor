# Battery Monitor

[![CI](https://github.com/bdkabiruddin/battery-monitor/actions/workflows/ci.yml/badge.svg)](https://github.com/bdkabiruddin/battery-monitor/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/License-MIT-yellow.svg)](LICENSE)

A live battery dashboard for Linux laptops: charge status, voltage/current/
power/temperature telemetry, health diagnostics, per-process power usage,
charging controls, alerts, and long-term history, backed by a local SQLite
database (`~/.local/share/battery-monitor/battery_history.db`).

This repo is a Tauri 2 native shell around a real HTML/CSS/JS frontend (no
bundler), with all 9 screens implemented — see
**[`ROADMAP.md`](ROADMAP.md)** for what's implemented per screen and
known gaps.

The UI is backed by **[`core/`](core)** (`battery-core`), a data-layer
library with no UI code of its own: `/sys/class/power_supply/BAT*/*`,
`ps`, `pkexec powertop`, `power-profiles-daemon`, GNOME Settings
Daemon/UPower, USB-C Power Delivery sysfs, and the SQLite history store.
Nothing in the app fabricates data — a feature either shows a real
reading or an honest "—"/"not implemented" placeholder, and every
omission is explained inline rather than silently missing.

## Building

```sh
git clone https://github.com/bdkabiruddin/battery-monitor
cd battery-monitor

# Needs the Tauri Linux prerequisites: webkit2gtk-4.1-dev,
# libayatana-appindicator3-dev, librsvg2-dev, build-essential, libssl-dev
cd app && cargo run
```

## Project structure

```
core/                   battery-core: the data layer (no UI)
app/                     Tauri 2 Rust backend
frontend/                HTML/CSS/JS frontend (no build step)
.github/                CI workflow, issue/PR templates
SECURITY.md             Vulnerability reporting, privilege/network scope
```

## License

[MIT](LICENSE)
