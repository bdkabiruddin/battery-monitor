use regex::Regex;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct CpuProcess {
    pub pid: String,
    pub name: String,
    pub cpu_percent: String,
    pub mem_percent: String,
    pub runtime: String,
}

/// Ranks running processes by %CPU via `ps` -- no root needed, unlike
/// powertop's RAPL-based per-process power estimates (see
/// parse_powertop_html / the "PowerTop Scan" option for those).
/// Sustained CPU usage is a reasonable proxy for power draw for most
/// workloads, though it won't catch wakeup-heavy-but-low-CPU offenders the
/// way powertop does.
///
/// Returns [] if `ps` itself isn't available or fails -- same
/// degrade-quietly philosophy as the rest of this crate's I/O boundaries.
pub fn read_top_processes_by_cpu(limit: usize) -> Vec<CpuProcess> {
    let output = Command::new("ps")
        .args(["-eo", "pid,comm,%cpu,%mem,etime", "--sort=-%cpu", "--no-headers"])
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return Vec::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut rows = Vec::new();
    for line in stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() != 5 {
            continue;
        }
        let (pid, name, cpu_str, mem_str, etime) = (parts[0], parts[1], parts[2], parts[3], parts[4]);
        let (cpu, mem) = match (cpu_str.parse::<f64>(), mem_str.parse::<f64>()) {
            (Ok(c), Ok(m)) => (c, m),
            _ => continue,
        };
        rows.push(CpuProcess {
            pid: pid.to_string(),
            name: name.to_string(),
            cpu_percent: format!("{cpu:.1}%"),
            mem_percent: format!("{mem:.1}%"),
            runtime: etime.to_string(),
        });
        if rows.len() >= limit {
            break;
        }
    }
    rows
}

#[derive(Debug, Clone, PartialEq)]
pub struct PowerTopProcess {
    pub pid: Option<String>,
    pub name: String,
    pub impact: String,
    pub category: String,
}

/// Runs `pkexec powertop --html=<tmpfile> --time=<seconds>` and parses the
/// resulting report. This is the only pkexec-gated (root-requiring)
/// operation in this crate -- callers must invoke it explicitly (e.g. a
/// manual "Scan" button), never from a background poll loop, to avoid
/// repeated password prompts.
pub fn run_powertop_scan(seconds: u32, limit: usize) -> Result<Vec<PowerTopProcess>, String> {
    let tmp = create_owned_temp_file("battery-monitor-powertop", "html")?;
    let status = Command::new("pkexec")
        .arg("powertop")
        .arg(format!("--html={}", tmp.display()))
        .arg(format!("--time={seconds}"))
        .status()
        .map_err(|e| format!("failed to run powertop: {e}"))?;
    if !status.success() {
        return Err("powertop scan did not complete (cancelled or failed)".to_string());
    }
    let rows = parse_powertop_html(&tmp, limit);
    let _ = std::fs::remove_file(&tmp);
    Ok(rows)
}

/// Only entries powertop itself labels as a process get a PID here (e.g.
/// "[PID 213904] jolt") -- plain bracketed numbers without "PID" are
/// kernel/IRQ identifiers (e.g. "[224] i915", "[7] sched(softirq)"), not
/// process IDs, and are deliberately left unmatched rather than guessed.
fn powertop_pid_re() -> Regex {
    Regex::new(r"\[PID (\d+)\]").unwrap()
}

/// Pulls the number and unit out of an impact string like "26.3 ms/s" or
/// "617.3 us/s" for sorting. powertop switches to us/s (microseconds/s)
/// for small values rather than rounding a ms/s figure down to 0.0, so the
/// raw number alone isn't comparable across rows -- 900 us/s (0.9 ms/s)
/// must sort *below* 5.0 ms/s, not above it.
fn impact_value_re() -> Regex {
    Regex::new(r"([\d.]+)\s*(ms/s|[uµ]s/s)?").unwrap()
}

fn impact_sort_key(re: &Regex, impact: &str) -> f64 {
    let Some(caps) = re.captures(impact) else { return 0.0 };
    let Ok(value) = caps.get(1).map(|m| m.as_str()).unwrap_or("").parse::<f64>() else {
        return 0.0;
    };
    let unit = caps.get(2).map(|m| m.as_str().to_lowercase()).unwrap_or_default();
    if unit == "us/s" || unit == "µs/s" {
        value / 1000.0
    } else {
        value
    }
}

/// Pulls rows out of powertop's "Overview of Software Power Consumers"
/// table in its --html report, filtered to Category == "Process" (the
/// panel shows actionable processes, not kernel/IRQ/device wakeup sources
/// -- those don't have a PID to act on) and sorted by descending impact.
pub fn parse_powertop_html(path: &Path, limit: usize) -> Vec<PowerTopProcess> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };

    let raw_rows = extract_table_rows(&text);
    let pid_re = powertop_pid_re();
    let impact_re = impact_value_re();

    let mut rows: Vec<PowerTopProcess> = raw_rows
        .into_iter()
        .filter_map(|cells| {
            if cells.len() < 2 {
                return None;
            }
            let impact = cells[0].clone();
            let description = cells[cells.len() - 1].clone();
            let category = if cells.len() >= 3 {
                cells[cells.len() - 2].clone()
            } else {
                String::new()
            };
            if category.trim().to_lowercase() != "process" {
                return None;
            }
            let (pid, name) = match pid_re.captures(&description) {
                Some(m) => {
                    let whole = m.get(0).unwrap();
                    let pid = m.get(1).unwrap().as_str().to_string();
                    let name = format!("{}{}", &description[..whole.start()], &description[whole.end()..]);
                    (Some(pid), name.trim().to_string())
                }
                None => (None, description),
            };
            Some(PowerTopProcess { pid, name, impact, category })
        })
        .collect();

    rows.sort_by(|a, b| {
        impact_sort_key(&impact_re, &b.impact)
            .partial_cmp(&impact_sort_key(&impact_re, &a.impact))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    rows.truncate(limit);
    rows
}

/// Minimal streaming HTML table extractor, mirroring the Python
/// HTMLParser-based `_PowerTopTableParser`: finds the *first* `<table>`
/// following a heading (`h1`/`h2`/`h3`) whose text contains "software
/// power consumers", and yields each `<tr>` that has at least one `<td>`
/// (skips pure-`<th>` header rows) as a list of cell texts.
fn extract_table_rows(html: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();

    let mut in_heading = false;
    let mut heading_text = String::new();
    let mut seen_heading_recently = false;

    let mut table_depth = 0i32;
    let mut in_target_table = false;

    let mut in_row = false;
    let mut row_cells: Vec<String> = Vec::new();
    let mut row_has_td = false;

    let mut in_cell = false;
    let mut cell_text = String::new();

    let bytes = html.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'<' {
            if let Some(end) = html[i..].find('>') {
                let tag_src = &html[i + 1..i + end];
                let end_idx = i + end;
                let is_close = tag_src.starts_with('/');
                let name_part = tag_src.trim_start_matches('/');
                let tag_name: String = name_part
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric())
                    .collect::<String>()
                    .to_lowercase();

                if !is_close {
                    match tag_name.as_str() {
                        "h1" | "h2" | "h3" => {
                            in_heading = true;
                            heading_text.clear();
                        }
                        "table" => {
                            table_depth += 1;
                            if seen_heading_recently && !in_target_table {
                                in_target_table = true;
                            }
                        }
                        "tr" if in_target_table => {
                            in_row = true;
                            row_cells.clear();
                            row_has_td = false;
                        }
                        "td" | "th" if in_row => {
                            in_cell = true;
                            cell_text.clear();
                            if tag_name == "td" {
                                row_has_td = true;
                            }
                        }
                        _ => {}
                    }
                } else {
                    match tag_name.as_str() {
                        "h1" | "h2" | "h3" if in_heading => {
                            in_heading = false;
                            seen_heading_recently =
                                heading_text.trim().to_lowercase().contains("software power consumers");
                        }
                        "td" | "th" if in_cell => {
                            in_cell = false;
                            row_cells.push(decode_entities(cell_text.trim()));
                        }
                        "tr" if in_row => {
                            in_row = false;
                            if row_has_td && row_cells.len() >= 2
                                && !row_cells[0].is_empty() && !row_cells.last().unwrap().is_empty() {
                                    rows.push(row_cells.clone());
                                }
                        }
                        "table" => {
                            table_depth -= 1;
                            if in_target_table && table_depth == 0 {
                                in_target_table = false;
                            }
                        }
                        _ => {}
                    }
                }
                i = end_idx + 1;
                continue;
            }
        }

        // Text node: accumulate into whichever buffer is active.
        let next_lt = html[i..].find('<').map(|p| i + p).unwrap_or(bytes.len());
        let text = &html[i..next_lt];
        if in_cell {
            cell_text.push_str(text);
        } else if in_heading {
            heading_text.push_str(text);
        }
        i = next_lt;
    }

    rows
}

/// Creates an unpredictable temp path and claims it atomically
/// (`create_new` fails if anything -- including a pre-planted symlink --
/// already exists there), before handing it to the root-elevated
/// `powertop` process below. Without this, a guessable `/tmp` path plus a
/// pre-placed symlink would let a local attacker make the root-elevated
/// write follow the symlink and clobber a file they can't write directly.
/// The file itself is left owned by the calling user, so powertop's own
/// `--html=` write just overwrites the regular file we already hold --
/// there's nothing left for it to follow.
fn create_owned_temp_file(prefix: &str, ext: &str) -> Result<std::path::PathBuf, String> {
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let marker = Box::new(0u8); // stack/heap address adds ASLR entropy
        let entropy = nanos ^ ((std::process::id() as u128) << 64) ^ (&*marker as *const u8 as u128);
        let path = std::env::temp_dir().join(format!("{prefix}-{entropy:x}.{ext}"));
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("failed to create temp file: {e}")),
        }
    }
    Err("failed to allocate a unique temp path".to_string())
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_process_rows_from_minimal_report() {
        let html = r#"
            <html><body>
            <h2>Overview of Software Power Consumers</h2>
            <table>
                <tr><th>Usage</th><th>Category</th><th>Description</th></tr>
                <tr><td>26.3 ms/s</td><td>Process</td><td>[PID 1234] firefox</td></tr>
                <tr><td>900 us/s</td><td>Process</td><td>[PID 5678] chrome</td></tr>
                <tr><td>5.0 ms/s</td><td>Device</td><td>Wi-Fi adapter</td></tr>
            </table>
            </body></html>
        "#;
        let dir = std::env::temp_dir().join(format!("battery-core-powertop-test-{}", std::process::id()));
        std::fs::write(&dir, html).unwrap();
        let rows = parse_powertop_html(&dir, 10);
        std::fs::remove_file(&dir).ok();

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid.as_deref(), Some("1234"));
        assert_eq!(rows[0].name, "firefox");
        // 26.3 ms/s > 900 us/s (0.9 ms/s) -- must sort first.
        assert_eq!(rows[1].pid.as_deref(), Some("5678"));
    }

    #[test]
    fn no_heading_yields_no_rows() {
        let html = "<html><body><table><tr><td>1</td><td>Process</td><td>x</td></tr></table></body></html>";
        let dir = std::env::temp_dir().join(format!("battery-core-powertop-test2-{}", std::process::id()));
        std::fs::write(&dir, html).unwrap();
        let rows = parse_powertop_html(&dir, 10);
        std::fs::remove_file(&dir).ok();
        assert!(rows.is_empty());
    }

    #[test]
    fn impact_sort_key_normalizes_units() {
        let re = impact_value_re();
        assert!(impact_sort_key(&re, "26.3 ms/s") > impact_sort_key(&re, "900 us/s"));
        assert!((impact_sort_key(&re, "5.0 ms/s") - 5.0).abs() < 1e-9);
        assert!((impact_sort_key(&re, "500 us/s") - 0.5).abs() < 1e-9);
    }
}
