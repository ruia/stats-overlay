//! Reading and editing %USERPROFILE%\.wslconfig. Changes apply the next time the
//! WSL VM starts. Edits are done line by line so comments, other settings and line
//! endings stay as they were, and the original is backed up once beforehand.

use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Reclaim {
    NotSet,
    Gradual,
    DropCache,
    Disabled,
    Unknown,
}

impl Reclaim {
    pub fn label(self) -> &'static str {
        match self {
            Reclaim::NotSet => "not set (WSL default)",
            Reclaim::Gradual => "gradual",
            Reclaim::DropCache => "dropCache",
            Reclaim::Disabled => "disabled",
            Reclaim::Unknown => "unrecognised value",
        }
    }

    /// The value written to the file; None removes the key.
    pub fn value(self) -> Option<&'static str> {
        match self {
            Reclaim::Gradual => Some("gradual"),
            Reclaim::DropCache => Some("dropCache"),
            Reclaim::Disabled => Some("disabled"),
            Reclaim::NotSet | Reclaim::Unknown => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
pub struct WslConfig {
    pub exists: bool,
    /// [experimental] autoMemoryReclaim (also accepted under [wsl2]).
    pub reclaim: Reclaim,
    /// [wsl2] memory, in bytes. None: WSL's default (50% of RAM).
    pub memory: Option<u64>,
}

impl Default for WslConfig {
    fn default() -> Self {
        WslConfig {
            exists: false,
            reclaim: Reclaim::NotSet,
            memory: None,
        }
    }
}

pub fn path() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").map(|h| PathBuf::from(h).join(".wslconfig"))
}

pub fn read() -> WslConfig {
    match path().and_then(|p| std::fs::read_to_string(p).ok()) {
        Some(text) => WslConfig {
            exists: true,
            ..parse(&text)
        },
        None => WslConfig::default(),
    }
}

/// `(section, key, value)` for every setting, comments stripped.
fn entries(text: &str) -> impl Iterator<Item = (String, &str, &str)> {
    let mut section = String::new();
    text.lines().filter_map(move |raw| {
        let line = raw.split(['#', ';']).next().unwrap_or("").trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.trim().to_ascii_lowercase();
            return None;
        }
        let (k, v) = line.split_once('=')?;
        Some((section.clone(), k.trim(), v.trim().trim_matches('"')))
    })
}

fn parse(text: &str) -> WslConfig {
    let mut c = WslConfig::default();
    for (section, key, value) in entries(text) {
        if key.eq_ignore_ascii_case("autoMemoryReclaim")
            && (section == "experimental" || section == "wsl2")
        {
            c.reclaim = match value.to_ascii_lowercase().as_str() {
                "gradual" => Reclaim::Gradual,
                "dropcache" => Reclaim::DropCache,
                "disabled" => Reclaim::Disabled,
                _ => Reclaim::Unknown,
            };
        } else if key.eq_ignore_ascii_case("memory") && section == "wsl2" {
            c.memory = parse_size(value);
        }
    }
    c
}

/// "24GB", "24G", "16384MB", "512mb" → bytes.
fn parse_size(v: &str) -> Option<u64> {
    let v = v.trim().to_ascii_uppercase();
    let digits: String = v
        .chars()
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    let n: f64 = digits.parse().ok()?;
    let unit = v[digits.len()..].trim().trim_end_matches('B');
    let mult = match unit {
        "" => 1.0,
        "K" => 1024.0,
        "M" => 1024.0 * 1024.0,
        "G" => 1024.0 * 1024.0 * 1024.0,
        "T" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mult) as u64)
}

/// Sets autoMemoryReclaim, in whichever section already has it (newer WSL also
/// reads it from [wsl2]), or in [experimental].
pub fn set_reclaim(r: Reclaim) -> std::io::Result<()> {
    let text = path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .unwrap_or_default();
    let in_wsl2 =
        entries(&text).any(|(s, k, _)| s == "wsl2" && k.eq_ignore_ascii_case("autoMemoryReclaim"));
    set(
        if in_wsl2 { "wsl2" } else { "experimental" },
        "autoMemoryReclaim",
        r.value(),
    )
}

/// Sets [wsl2] memory in whole GB, or removes it (WSL's default) for None.
pub fn set_memory(gb: Option<u32>) -> std::io::Result<()> {
    set("wsl2", "memory", gb.map(|g| format!("{g}GB")).as_deref())
}

/// Sets `key` in `[section]` to `value`, or removes it when `value` is None.
fn set(section: &str, key: &str, value: Option<&str>) -> std::io::Result<()> {
    let path = path().ok_or_else(|| std::io::Error::other("USERPROFILE not set"))?;
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() {
        let backup = path.with_file_name(".wslconfig.statsoverlay-backup");
        if !backup.exists() {
            std::fs::write(backup, &text)?;
        }
    }
    std::fs::write(&path, edit(&text, section, key, value))
}

fn edit(text: &str, section: &str, key: &str, value: Option<&str>) -> String {
    let nl = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut current = String::new();
    let mut section_end: Option<usize> = None; // index after the section's last setting line
    let mut found = None;
    for (i, raw) in lines.iter().enumerate() {
        let line = raw.split(['#', ';']).next().unwrap_or("").trim();
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            current = s.trim().to_ascii_lowercase();
            if current == section {
                section_end = Some(i + 1);
            }
            continue;
        }
        if current == section {
            if !line.is_empty() {
                section_end = Some(i + 1);
            }
            if line
                .split_once('=')
                .is_some_and(|(k, _)| k.trim().eq_ignore_ascii_case(key))
            {
                found = Some(i);
            }
        }
    }
    match (found, value) {
        (Some(i), Some(v)) => lines[i] = format!("{key}={v}"),
        (Some(i), None) => {
            lines.remove(i);
        }
        (None, Some(v)) => match section_end {
            Some(at) => lines.insert(at, format!("{key}={v}")),
            None => {
                if lines.last().is_some_and(|l| !l.trim().is_empty()) {
                    lines.push(String::new());
                }
                lines.push(format!("[{section}]"));
                lines.push(format!("{key}={v}"));
            }
        },
        (None, None) => {}
    }
    let mut out = lines.join(nl);
    out.push_str(nl);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses() {
        let c = parse("[wsl2]\nmemory=24GB # cap\n[experimental]\nautoMemoryReclaim=dropCache\n");
        assert_eq!(c.reclaim, Reclaim::DropCache);
        assert_eq!(c.memory, Some(24 << 30));
        assert_eq!(
            parse("[general]\nautoMemoryReclaim=gradual\n").reclaim,
            Reclaim::NotSet
        );
        assert_eq!(parse_size("16384MB"), Some(16 << 30));
    }

    #[test]
    fn edits() {
        let t = "[wsl2]\r\nmemory=8GB\r\n\r\n[experimental]\r\nsparseVhd=true\r\n";
        assert_eq!(
            edit(t, "wsl2", "memory", Some("24GB")),
            "[wsl2]\r\nmemory=24GB\r\n\r\n[experimental]\r\nsparseVhd=true\r\n"
        );
        assert_eq!(edit(t, "experimental", "autoMemoryReclaim", Some("gradual")), "[wsl2]\r\nmemory=8GB\r\n\r\n[experimental]\r\nsparseVhd=true\r\nautoMemoryReclaim=gradual\r\n");
        assert_eq!(
            edit(t, "wsl2", "memory", None),
            "[wsl2]\r\n\r\n[experimental]\r\nsparseVhd=true\r\n"
        );
        assert_eq!(
            edit("", "experimental", "autoMemoryReclaim", Some("gradual")),
            "[experimental]\nautoMemoryReclaim=gradual\n"
        );
        assert_eq!(
            edit(
                "[wsl2]\nswap=0\n",
                "experimental",
                "autoMemoryReclaim",
                Some("gradual")
            ),
            "[wsl2]\nswap=0\n\n[experimental]\nautoMemoryReclaim=gradual\n"
        );
    }
}
