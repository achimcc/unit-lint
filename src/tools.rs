//! `tools`: is every program that a hand or the guest check starts in a
//! guest actually in that guest's system profile?
//!
//! A unit's `ExecStart` carries the store path, so the unit runs; a program
//! started over the PATH (`systemd-run --machine … prog`, `vantage run guest
//! -- prog`) needs `environment.systemPackages`. The guest check and the
//! runbooks are our own sources, so the call sites can be read exactly.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::report::Finding;
use crate::unit;

/// One place in our sources that starts `program` in `machine` over the PATH.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSite {
    pub machine: String,
    pub program: String,
    pub file: PathBuf,
    pub line: usize,
}

/// Shell builtins that reach `a.gast(…)` as a "program" but are no file.
pub const BUILTINS: &[&str] = &[
    "test", "[", "cd", "echo", "true", "false", "exit", "printf", "read", "set", "export",
    "source", ".",
];

const RUST_ROOT: &str = "pkgs/gast-pruefung/src/gaeste";

/// Markdown a hand reads today. Dated documents are history and stay out.
const MARKDOWN_FILES: &[&str] = &["README.md", "CLAUDE.md"];
const MARKDOWN_DIRS: &[&str] = &[
    "docs/runbooks",
    "docs/werkzeuge",
    ".claude/skills",
    ".claude/agents",
];

static RUST_CALLS: LazyLock<[(Regex, usize); 3]> = LazyLock::new(|| {
    let guest = r#"(?:"([a-z0-9-]+)"|[A-Za-z_][A-Za-z0-9_]*)"#;
    [
        // a.gast(GAST, "prog", …) — program is the second argument.
        (
            Regex::new(&format!(r#"(?s)\ba\.gast\(\s*{guest}\s*,\s*"([^"]+)""#)).unwrap(),
            2,
        ),
        // a.gast_mit(GAST, &[…], "prog", …) — third.
        (
            Regex::new(&format!(
                r#"(?s)\ba\.gast_mit\(\s*{guest}\s*,\s*&\[[^\]]*\]\s*,\s*"([^"]+)""#
            ))
            .unwrap(),
            2,
        ),
        // a.als(GAST, "user", "group", "prog", …) — fourth.
        (
            Regex::new(&format!(
                r#"(?s)\ba\.als\(\s*{guest}\s*,\s*"[^"]*"\s*,\s*"[^"]*"\s*,\s*"([^"]+)""#
            ))
            .unwrap(),
            2,
        ),
    ]
});

static VANTAGE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"vantage run ([a-z0-9]+-[0-9]{2})\b[^\n]*?\s--\s+([^\s'"`]+)"#).unwrap()
});

/// Everything from `//` on, outside string literals, becomes spaces — the
/// byte offsets (and so the line numbers) stay where they were.
fn strip_line_comments(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for line in text.split_inclusive('\n') {
        let mut in_str = false;
        let mut escaped = false;
        let mut cut = line.len();
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let c = bytes[i];
            if in_str {
                if escaped {
                    escaped = false;
                } else if c == b'\\' {
                    escaped = true;
                } else if c == b'"' {
                    in_str = false;
                }
            } else if c == b'"' {
                in_str = true;
            } else if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
                cut = i;
                break;
            }
            i += 1;
        }
        out.push_str(&line[..cut]);
        for c in line[cut..].chars() {
            out.push(if c == '\n' { '\n' } else { ' ' });
        }
    }
    out
}

fn line_of(text: &str, offset: usize) -> usize {
    text[..offset].matches('\n').count() + 1
}

fn wanted(program: &str) -> bool {
    !program.starts_with('/') && !BUILTINS.contains(&program)
}

fn walk(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<Result<_, _>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let p = e.path();
        if p.is_dir() {
            walk(&p, ext, out)?;
        } else if p.extension().is_some_and(|x| x == ext) {
            out.push(p);
        }
    }
    Ok(())
}

fn guest_from_module(root: &Path, file: &Path) -> Option<String> {
    let rel = file.strip_prefix(root.join(RUST_ROOT)).ok()?;
    let first = rel.components().next()?.as_os_str().to_str()?;
    let name = first.strip_suffix(".rs").unwrap_or(first);
    Some(name.replace('_', "-"))
}

fn scan_rust(root: &Path, out: &mut Vec<CallSite>) -> std::io::Result<()> {
    let mut files = Vec::new();
    walk(&root.join(RUST_ROOT), "rs", &mut files)?;
    for file in files {
        let Some(module_guest) = guest_from_module(root, &file) else {
            continue;
        };
        let text = strip_line_comments(&std::fs::read_to_string(&file)?);
        let mut found: Vec<(usize, CallSite)> = Vec::new();
        for (re, prog_group) in RUST_CALLS.iter() {
            for c in re.captures_iter(&text) {
                let program = &c[*prog_group];
                if !wanted(program) {
                    continue;
                }
                let machine = c
                    .get(1)
                    .map_or_else(|| module_guest.clone(), |m| m.as_str().to_owned());
                let start = c.get(0).unwrap().start();
                found.push((
                    start,
                    CallSite {
                        machine,
                        program: program.to_owned(),
                        file: file.clone(),
                        line: line_of(&text, start),
                    },
                ));
            }
        }
        found.sort_by_key(|(start, _)| *start);
        out.extend(found.into_iter().map(|(_, s)| s));
    }
    Ok(())
}

fn scan_markdown(root: &Path, out: &mut Vec<CallSite>) -> std::io::Result<()> {
    let mut files: Vec<PathBuf> = MARKDOWN_FILES
        .iter()
        .map(|f| root.join(f))
        .filter(|p| p.is_file())
        .collect();
    for d in MARKDOWN_DIRS {
        walk(&root.join(d), "md", &mut files)?;
    }
    for file in files {
        let text = std::fs::read_to_string(&file)?;
        for (i, line) in text.lines().enumerate() {
            for c in VANTAGE_LINE.captures_iter(line) {
                let program = &c[2];
                if !wanted(program) {
                    continue;
                }
                out.push(CallSite {
                    machine: c[1].to_owned(),
                    program: program.to_owned(),
                    file: file.clone(),
                    line: i + 1,
                });
            }
        }
    }
    Ok(())
}

/// Every call site under `root`: the guest check's modules, and — unless
/// `rust_only` — the living Markdown.
pub fn scan_sources(root: &Path, rust_only: bool) -> std::io::Result<Vec<CallSite>> {
    let mut out = Vec::new();
    scan_rust(root, &mut out)?;
    if !rust_only {
        scan_markdown(root, &mut out)?;
    }
    Ok(out)
}

/// The rule id of every finding `tools` produces.
pub const TOOL_RULE: &str = "tool-missing";

fn present(profile_bin: &Path, program: &str) -> bool {
    std::fs::symlink_metadata(profile_bin.join(program)).is_ok_and(|m| !m.is_dir())
}

fn places(sites: &[&CallSite]) -> String {
    sites
        .iter()
        .map(|s| format!("{}:{}", s.file.display(), s.line))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Checks every call site against the guest it names. Returns the findings
/// and the names of the guests the toplevel has (for the staleness check of
/// exceptions). Several sites of one `(guest, program)` pair fold into one
/// finding that lists them all.
pub fn check(top: &Path, sites: &[CallSite]) -> std::io::Result<(Vec<Finding>, Vec<String>)> {
    let guests = unit::container_paths(top)?;
    let names: Vec<String> = guests.iter().map(|(n, _)| n.clone()).collect();
    // Group by (machine, program), keeping first-seen order.
    let mut groups: Vec<((String, String), Vec<&CallSite>)> = Vec::new();
    for s in sites {
        let key = (s.machine.clone(), s.program.clone());
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(s),
            None => groups.push((key, vec![s])),
        }
    }
    let mut findings = Vec::new();
    for ((machine, program), group) in groups {
        let Some((_, sp)) = guests.iter().find(|(n, _)| *n == machine) else {
            findings.push(Finding {
                rule: TOOL_RULE,
                system: machine.clone(),
                unit: program.clone(),
                message: format!(
                    "{program} is called from {} for machine {machine}, which is not a container of this toplevel",
                    places(&group)
                ),
            });
            continue;
        };
        if present(&sp.join("sw/bin"), &program) {
            continue;
        }
        findings.push(Finding {
            rule: TOOL_RULE,
            system: machine.clone(),
            unit: program.clone(),
            message: format!(
                "{program} is called from {} but is not in the system profile of {machine} (environment.systemPackages)",
                places(&group)
            ),
        });
    }
    Ok((findings, names))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let p =
                std::env::temp_dir().join(format!("unit-lint-tools-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&p);
            fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn file(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(&p, text).unwrap();
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn pairs(sites: &[CallSite]) -> Vec<(String, String)> {
        sites
            .iter()
            .map(|s| (s.machine.clone(), s.program.clone()))
            .collect()
    }

    #[test]
    fn multiline_als_and_literal_guest_and_comments() {
        let d = Dir::new("rust");
        d.file(
            "pkgs/gast-pruefung/src/gaeste/jelly_01/geraet.rs",
            "let e = a.als(\n    GAST,\n    \"jellyfin\",\n    \"render\",\n    \"vainfo\",\n    &[],\n); // a.gast(GAST, \"socat\", &[], None)\n\
             a.gast(\"auth-01\", \"authentik-kurztoken\", &[], None);\n\
             a.gast(GAST, \"/nix/store/abc/bin/x\", &[], None);\n\
             a.gast(GAST, \"test\", &[\"-f\", \"x\"], None);\n",
        );
        let sites = scan_sources(&d.0, true).unwrap();
        assert_eq!(
            pairs(&sites),
            vec![
                ("jelly-01".to_owned(), "vainfo".to_owned()),
                ("auth-01".to_owned(), "authentik-kurztoken".to_owned()),
            ]
        );
        assert_eq!(sites[0].line, 1);
        assert!(sites[0].file.ends_with("geraet.rs"));
    }

    #[test]
    fn gast_mit_third_argument() {
        let d = Dir::new("gastmit");
        d.file(
            "pkgs/gast-pruefung/src/gaeste/media_01/arr.rs",
            "a.gast_mit(\n GAST,\n &[\"LoadCredential=x\"],\n \"wc\",\n &[\"-c\"],\n)",
        );
        let sites = scan_sources(&d.0, true).unwrap();
        assert_eq!(
            pairs(&sites),
            vec![("media-01".to_owned(), "wc".to_owned())]
        );
    }

    #[test]
    fn markdown_living_docs_only() {
        let d = Dir::new("md");
        d.file(
            "docs/runbooks/x.md",
            "Prose: use `vantage run --header-file` for keys.\n\n```bash\nssh root@server vantage run obs-01 --header-file 'A=/x' -- curl -sS http://10.0.20.12:9090/\n```\n",
        );
        d.file(
            "docs/2026-01-01-alt.md",
            "`vantage run obs-01 -- logcli labels`\n",
        );
        let sites = scan_sources(&d.0, false).unwrap();
        assert_eq!(
            pairs(&sites),
            vec![("obs-01".to_owned(), "curl".to_owned())]
        );
        assert!(sites[0].file.ends_with("docs/runbooks/x.md"));
        assert!(scan_sources(&d.0, true).unwrap().is_empty());
    }

    #[test]
    fn no_sites_is_empty_not_error() {
        let d = Dir::new("empty");
        assert!(scan_sources(&d.0, false).unwrap().is_empty());
    }
}
