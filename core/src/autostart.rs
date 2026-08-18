use std::path::PathBuf;

/// XDG autostart entry, user-scoped (`~/.config/autostart`) -- no root
/// needed, unlike a `.deb` package's `/etc/xdg/autostart` entry.
fn autostart_path() -> Option<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".config")))?;
    Some(config.join("autostart").join("battery-monitor.desktop"))
}

pub fn is_enabled() -> bool {
    autostart_path().is_some_and(|p| p.exists())
}

/// Points the autostart entry at the currently running binary -- during
/// development that's `target/debug/battery-monitor-tauri`, not an
/// installed path, which is the honest thing to do until this rewrite has
/// its own packaging.
pub fn enable() -> Result<(), String> {
    let path = autostart_path().ok_or("couldn't determine XDG config directory")?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let contents = format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=Battery Monitor\n\
         Comment=Battery health, telemetry, and power-usage dashboard\n\
         Exec={}\n\
         Terminal=false\n\
         Categories=Utility;\n\
         X-GNOME-Autostart-enabled=true\n",
        exe.display()
    );
    std::fs::write(&path, contents).map_err(|e| e.to_string())
}

pub fn disable() -> Result<(), String> {
    let Some(path) = autostart_path() else {
        return Ok(());
    };
    if path.exists() {
        std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    }
    Ok(())
}
