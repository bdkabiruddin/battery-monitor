# Flatpak packaging

Local build/test:

```sh
flatpak remote-add --user --if-not-exists flathub https://flathub.org/repo/flathub.flatpakrepo
flatpak install --user flathub org.gnome.Platform//49 org.gnome.Sdk//49 org.freedesktop.Sdk.Extension.rust-stable//25.08

# From the repo root:
flatpak-builder --user --force-clean --install /tmp/bm-build-dir flatpak/io.github.bdkabiruddin.BatteryMonitor.yml
```

`cargo-sources.json` is a generated offline vendoring manifest (via
[`flatpak-cargo-generator.py`](https://github.com/flatpak/flatpak-builder-tools/tree/master/cargo))
that lets `cargo build` run with no network access inside the sandbox,
which Flathub's build servers require. Regenerate it whenever
`app/Cargo.lock` changes:

```sh
python3 flatpak-cargo-generator.py ../app/Cargo.lock -o flatpak/cargo-sources.json
```

## Known limitation: pkexec-gated features

The sandbox's `--filesystem=host` grants real read access to
`/sys/class/power_supply/*`, USB-C PD sysfs, RAPL's powercap sysfs,
etc., so the core dashboard (telemetry, health, alerts, history,
devices) works normally.

Features that shell out to `pkexec` (PowerTop scan, CPU package power
via RAPL, charge threshold writes) do not currently work inside the
sandbox -- `pkexec` can't reach across the Flatpak boundary to a host
process the way it can when this app is installed as a `.deb`/AppImage.
Fixing this means detecting `FLATPAK_ID` in the Rust source and routing
those calls through `flatpak-spawn --host pkexec ...` instead of
`pkexec` directly -- not yet done. Those features show their real
"cancelled or failed" error rather than pretending to work.

## Local verification note

On this machine's Ubuntu 22.04 install, `flatpak-builder`'s final
`appstream-compose` step fails locally (`bwrap: execvp
appstream-compose: No such file or directory`) because that step runs
inside a `--nofilesystem=host:reset` sandbox looking for the tool
inside the app's own runtime (`org.gnome.Platform`), which doesn't
ship it, rather than the host's `/usr/bin` -- installing the host
`appstream-compose` package doesn't fix it, since the sandboxed
invocation can't see it either way. `flatpak-builder --build-only`
(which skips that finish/export step) completes cleanly, confirming
the actual build -- the ~1000 vendored Cargo dependencies compiling
fully offline, and the binary/desktop-file/icon/metainfo installing
correctly -- works. This is understood to be a local toolchain quirk,
not something expected to block a real submission on Flathub's own
build infrastructure.

## Submitting to Flathub

1. Fork [flathub/flathub](https://github.com/flathub/flathub) and
   follow their [app submission
   guide](https://docs.flathub.org/docs/for-app-authors/submission) --
   short version: open a PR against the `new-pr` branch adding a new
   repo named after the app ID
   (`io.github.bdkabiruddin.BatteryMonitor`) whose manifest is this
   directory's `.yml` file (or a `git` source pointing back at this
   repo's `flatpak/` directory).
2. Flathub's own CI builds and lints the manifest; address whatever it
   flags.
3. Once merged, updates are just new commits/tags to this repo plus
   bumping the `<release>` entry in the `.metainfo.xml`.
