# Security Policy

## Supported Versions

Only the latest [release](https://github.com/bdkabiruddin/battery-monitor/releases)
is supported. Please upgrade before reporting an issue.

## Reporting a Vulnerability

Please **do not** open a public GitHub issue for security vulnerabilities.

Instead, email **bd.kabiruddin@gmail.com** with:
- A description of the vulnerability and its potential impact
- Steps to reproduce it
- Any relevant logs (e.g. `journalctl --user -b 0 | grep -i battery`)

You should get a response within a few days. Once a fix is available, it
will be released and the report credited (unless you'd prefer otherwise).

## Scope notes

This is a local desktop utility: it reads `/sys/class/power_supply`, writes
to a local SQLite database under `~/.local/share/battery-monitor/`, and
(Tauri build only) talks to the session D-Bus for the system tray icon and
for `power-profiles-daemon`/GNOME Settings Daemon integration. It does not
make network requests, and does not run with elevated privileges except for
the optional, explicitly user-triggered PowerTop scan and charge-threshold
write (both via `pkexec`, prompting for a password each time). It does not
accept remote input. Reports involving local D-Bus exposure, sysfs parsing,
`pkexec`-gated operations, powertop HTML-report parsing, or SQLite handling
are all in scope.
