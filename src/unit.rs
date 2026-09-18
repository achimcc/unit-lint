//! Reading unit files the way systemd does — as far as the rules need it.
//!
//! A *system* is one directory of units: `etc/systemd/system` of a NixOS
//! toplevel, or of one of its containers. NixOS writes every unit as a
//! symlink into the store, a unit it only amends as a drop-in directory
//! `<name>.d/`, a masked one as a symlink to `/dev/null`, and the dependencies
//! of targets as `<target>.wants/` directories.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

const UNIT_SUFFIXES: &[&str] = &[
    ".service",
    ".timer",
    ".socket",
    ".target",
    ".path",
    ".mount",
    ".automount",
    ".swap",
    ".slice",
    ".scope",
];

pub fn is_unit_name(name: &str) -> bool {
    UNIT_SUFFIXES
        .iter()
        .any(|s| name.ends_with(s) && name.len() > s.len())
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub section: String,
    pub key: String,
    pub value: String,
}

#[derive(Debug, Default)]
pub struct Unit {
    pub name: String,
    pub masked: bool,
    /// The unit file itself, resolved. `None` for a unit that only has
    /// drop-ins.
    pub file: Option<PathBuf>,
    pub dropins: Vec<PathBuf>,
    /// Every assignment, in the order systemd applies them: the file, then
    /// the drop-ins sorted by name.
    pub entries: Vec<Entry>,
}

impl Unit {
    /// The effective value of a single-valued setting. An empty assignment
    /// resets it to the default, which is reported as `None`.
    pub fn last(&self, section: &str, key: &str) -> Option<&str> {
        let v = self
            .entries
            .iter()
            .rev()
            .find(|e| e.section == section && e.key == key)?;
        (!v.value.is_empty()).then_some(v.value.as_str())
    }

    /// The effective value of a list setting: assignments accumulate, an
    /// empty one clears what came before.
    pub fn list(&self, section: &str, key: &str) -> Vec<&str> {
        let mut out = Vec::new();
        for e in self
            .entries
            .iter()
            .filter(|e| e.section == section && e.key == key)
        {
            if e.value.is_empty() {
                out.clear();
            } else {
                out.push(e.value.as_str());
            }
        }
        out
    }

    pub fn has(&self, section: &str, key: &str) -> bool {
        !self.list(section, key).is_empty()
    }

    pub fn kind(&self) -> &str {
        self.name.rsplit('.').next().unwrap_or("")
    }

    /// `foo@bar.service` -> `foo@.service`.
    pub fn template_name(name: &str) -> Option<String> {
        let (prefix, rest) = name.split_once('@')?;
        let suffix = rest.rsplit_once('.').map(|(_, s)| s)?;
        (!rest.starts_with('.')).then(|| format!("{prefix}@.{suffix}"))
    }
}

#[derive(Debug, Default)]
pub struct System {
    pub name: String,
    pub dir: PathBuf,
    /// A NixOS container rather than a host.
    pub container: bool,
    pub units: BTreeMap<String, Unit>,
    /// `<target>.wants/` and `.requires/` — which units a unit pulls in.
    pub wants: BTreeMap<String, BTreeSet<String>>,
}

impl System {
    /// A unit by name, falling back to its template for an instance.
    pub fn lookup(&self, name: &str) -> Option<&Unit> {
        self.units
            .get(name)
            .or_else(|| self.units.get(&Unit::template_name(name)?))
    }

    pub fn exists(&self, name: &str) -> bool {
        self.lookup(name)
            .is_some_and(|u| u.file.is_some() || u.masked)
    }

    pub fn pulled_in_by(&self, target: &str, unit: &str) -> bool {
        self.wants.get(target).is_some_and(|w| w.contains(unit))
    }
}

pub fn parse(text: &str, out: &mut Vec<Entry>) {
    let mut section = String::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if pending.is_empty() && (line.is_empty() || line.starts_with('#') || line.starts_with(';'))
        {
            continue;
        }
        // A trailing backslash continues the line; systemd joins with a space.
        if let Some(head) = line.strip_suffix('\\') {
            pending.push_str(head);
            pending.push(' ');
            continue;
        }
        pending.push_str(line);
        let full = std::mem::take(&mut pending);
        if let Some(name) = full.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            section = name.to_owned();
        } else if let Some((k, v)) = full.split_once('=') {
            out.push(Entry {
                section: section.clone(),
                key: k.trim().to_owned(),
                value: v.trim().to_owned(),
            });
        }
    }
}

fn read_units(path: &Path, out: &mut Vec<Entry>) -> std::io::Result<()> {
    parse(&fs::read_to_string(path)?, out);
    Ok(())
}

fn sorted_dir(path: &Path) -> std::io::Result<Vec<(String, PathBuf)>> {
    let mut v: Vec<_> = fs::read_dir(path)?
        .filter_map(Result::ok)
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    v.sort();
    Ok(v)
}

/// Loads one directory of units. In NixOS that directory is complete: the
/// systemd package's own unit directory is in the search path, but NixOS
/// ships its units under `example/` and links what it uses into
/// `etc/systemd/system` — a unit missing there is missing on the machine.
pub fn load(name: &str, dir: &Path, container: bool) -> std::io::Result<System> {
    let mut sys = System {
        name: name.to_owned(),
        dir: dir.to_owned(),
        container,
        ..System::default()
    };
    let mut dropin_dirs: BTreeMap<String, PathBuf> = BTreeMap::new();

    for (entry, path) in sorted_dir(dir)? {
        if let Some(target) = entry
            .strip_suffix(".wants")
            .or_else(|| entry.strip_suffix(".requires"))
        {
            if path.is_dir() {
                let set = sys.wants.entry(target.to_owned()).or_default();
                for (child, _) in sorted_dir(&path)? {
                    set.insert(child);
                }
            }
            continue;
        }
        if let Some(unit) = entry.strip_suffix(".d") {
            if path.is_dir() && is_unit_name(unit) {
                dropin_dirs.insert(unit.to_owned(), path);
            }
            continue;
        }
        if !is_unit_name(&entry) {
            continue;
        }
        let resolved = fs::canonicalize(&path).unwrap_or(path.clone());
        let u = sys.units.entry(entry.clone()).or_default();
        u.name = entry;
        if resolved == Path::new("/dev/null") {
            u.masked = true;
        } else if resolved.is_file() {
            u.file = Some(resolved);
        }
    }

    for (unit, dir) in &dropin_dirs {
        let u = sys.units.entry(unit.clone()).or_default();
        u.name = unit.clone();
        for (f, p) in sorted_dir(dir)? {
            if f.ends_with(".conf") {
                u.dropins.push(p);
            }
        }
    }

    let names: Vec<String> = sys.units.keys().cloned().collect();
    for name in names {
        let mut entries = Vec::new();
        let (file, own_dropins) = {
            let u = &sys.units[&name];
            (u.file.clone(), u.dropins.clone())
        };
        if let Some(f) = &file {
            read_units(f, &mut entries)?;
        }
        // An instance also takes the drop-ins of its template, first.
        let mut all_dropins = Vec::new();
        if let Some(t) = Unit::template_name(&name)
            && let Some(td) = dropin_dirs.get(&t)
        {
            for (f, p) in sorted_dir(td)? {
                if f.ends_with(".conf") {
                    all_dropins.push(p);
                }
            }
        }
        all_dropins.extend(own_dropins);
        for d in &all_dropins {
            read_units(d, &mut entries)?;
        }
        sys.units.get_mut(&name).expect("present").entries = entries;
    }
    Ok(sys)
}

/// A NixOS toplevel brings its containers along: each
/// `etc/nixos-containers/<name>.conf` names the container's own toplevel.
pub fn load_toplevel(label: &str, top: &Path) -> std::io::Result<Vec<System>> {
    let mut out = vec![load_one_toplevel(label, top, false)?];
    let confs = top.join("etc/nixos-containers");
    if confs.is_dir() {
        for (f, p) in sorted_dir(&confs)? {
            let Some(name) = f.strip_suffix(".conf") else {
                continue;
            };
            let text = fs::read_to_string(&p)?;
            let sp = text
                .lines()
                .find_map(|l| l.trim().strip_prefix("SYSTEM_PATH="))
                .map(|s| s.trim_matches('"'));
            match sp {
                Some(sp) if Path::new(sp).join("etc/systemd/system").is_dir() => {
                    out.push(load_one_toplevel(name, Path::new(sp), true)?);
                }
                _ => {
                    return Err(std::io::Error::other(format!(
                        "container {name}: no readable SYSTEM_PATH in {} (is the toplevel fully built?)",
                        p.display()
                    )));
                }
            }
        }
    }
    Ok(out)
}

fn load_one_toplevel(label: &str, top: &Path, container: bool) -> std::io::Result<System> {
    load(label, &top.join("etc/systemd/system"), container)
}

pub fn parse_bool(v: &str) -> Option<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "yes" | "y" | "true" | "t" | "on" => Some(true),
        "0" | "no" | "n" | "false" | "f" | "off" => Some(false),
        _ => None,
    }
}

/// A systemd time span in seconds; `None` is infinity.
pub fn parse_timespan(v: &str) -> Option<Option<f64>> {
    let v = v.trim();
    if v == "infinity" {
        return Some(None);
    }
    let mut total = 0.0;
    let mut rest = v;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let num: f64 = rest[..num_len].parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(rest.len());
        let factor = match &rest[..unit_len] {
            "" | "s" | "sec" | "second" | "seconds" => 1.0,
            "us" | "usec" | "µs" => 1e-6,
            "ms" | "msec" => 1e-3,
            "m" | "min" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86400.0,
            "w" | "week" | "weeks" => 604800.0,
            "M" | "month" | "months" => 2_629_800.0,
            "y" | "year" | "years" => 31_557_600.0,
            _ => return None,
        };
        total += num * factor;
        rest = &rest[unit_len..];
    }
    Some(Some(total))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_continuation_and_comments() {
        let mut e = Vec::new();
        parse(
            "# c\n[Service]\nExecStart=/bin/a \\\n  --flag\n; x\nEnvironment=A=1\n",
            &mut e,
        );
        assert_eq!(e[0].value, "/bin/a  --flag");
        assert_eq!(e[1].key, "Environment");
        assert_eq!(e[1].value, "A=1");
    }

    #[test]
    fn list_resets() {
        let mut u = Unit::default();
        parse(
            "[Service]\nExecStart=/a\nExecStart=\nExecStart=/b\n",
            &mut u.entries,
        );
        assert_eq!(u.list("Service", "ExecStart"), ["/b"]);
    }

    #[test]
    fn templates() {
        assert_eq!(
            Unit::template_name("mail@foo.service").as_deref(),
            Some("mail@.service")
        );
        assert_eq!(Unit::template_name("mail@.service"), None);
        assert_eq!(Unit::template_name("plain.service"), None);
    }

    #[test]
    fn timespans() {
        assert_eq!(parse_timespan("100ms"), Some(Some(0.1)));
        assert_eq!(parse_timespan("1min 30s"), Some(Some(90.0)));
        assert_eq!(parse_timespan("5"), Some(Some(5.0)));
        assert_eq!(parse_timespan("infinity"), Some(None));
        assert_eq!(parse_timespan("2h30min"), Some(Some(9000.0)));
        assert_eq!(parse_timespan("bogus"), None);
    }
}
