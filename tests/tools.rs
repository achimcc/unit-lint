//! `tools` against a built toplevel: call sites are checked against each
//! guest's `sw/bin`.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use unit_lint::tools::{self, CallSite};

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p =
            std::env::temp_dir().join(format!("unit-lint-toolstest-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
    fn file(&self, rel: &str, text: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, text).unwrap();
        p
    }
    fn dir(&self, rel: &str) -> PathBuf {
        let p = self.0.join(rel);
        fs::create_dir_all(&p).unwrap();
        p
    }
    fn link(&self, rel: &str, target: &Path) {
        let p = self.0.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        symlink(target, p).unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// A toplevel with two guests. `probe-01` has `curl` (a symlink, as NixOS
/// writes them) and a directory called `dirprog`; `other-01` has nothing.
fn toplevel(d: &Dir) -> PathBuf {
    let top = d.dir("top");
    d.dir("top/etc/systemd/system");
    for g in ["probe-01", "other-01"] {
        let sys = d.dir(&format!("guests/{g}"));
        d.dir(&format!("guests/{g}/etc/systemd/system"));
        d.dir(&format!("guests/{g}/sw/bin"));
        d.file(
            &format!("top/etc/nixos-containers/{g}.conf"),
            &format!("PRIVATE_NETWORK=1\nSYSTEM_PATH=\"{}\"\n", sys.display()),
        );
    }
    let real = d.file("store/curl-1/bin/curl", "#!/bin/sh\n");
    d.link("guests/probe-01/sw/bin/curl", &real);
    d.dir("guests/probe-01/sw/bin/dirprog");
    top
}

fn site(machine: &str, program: &str, file: &str, line: usize) -> CallSite {
    CallSite {
        machine: machine.into(),
        program: program.into(),
        file: PathBuf::from(file),
        line,
    }
}

#[test]
fn missing_programs_are_findings_present_ones_are_not() {
    let d = Dir::new("check");
    let top = toplevel(&d);
    let sites = vec![
        site("probe-01", "curl", "a.rs", 1),
        site("probe-01", "socat", "a.rs", 5),
        site("probe-01", "dirprog", "a.rs", 7),
        site("probe-01", "socat", "docs/runbooks/x.md", 3),
        site("ghost-01", "curl", "b.rs", 2),
    ];
    let (findings, systems) = tools::check(&top, &sites).unwrap();
    assert_eq!(systems, vec!["other-01".to_owned(), "probe-01".to_owned()]);
    let mut pairs: Vec<(String, String)> = findings
        .iter()
        .map(|f| (f.system.clone(), f.unit.clone()))
        .collect();
    pairs.sort();
    assert_eq!(
        pairs,
        vec![
            ("ghost-01".to_owned(), "curl".to_owned()),
            ("probe-01".to_owned(), "dirprog".to_owned()),
            ("probe-01".to_owned(), "socat".to_owned()),
            ("probe-01".to_owned(), "socat".to_owned()),
        ]
    );
    assert!(findings.iter().all(|f| f.rule == tools::TOOL_RULE));
    // One finding per source file, so an exception can name the file.
    let socat: Vec<_> = findings.iter().filter(|f| f.unit == "socat").collect();
    assert_eq!(socat.len(), 2);
    assert!(
        socat
            .iter()
            .any(|f| f.file.as_deref() == Some("a.rs") && f.message.contains("a.rs:5"))
    );
    assert!(
        socat.iter().any(
            |f| f.file.as_deref() == Some("docs/runbooks/x.md") && f.message.contains("x.md:3")
        )
    );
    assert!(socat[0].message.contains("environment.systemPackages"));
    let ghost = findings.iter().find(|f| f.system == "ghost-01").unwrap();
    assert!(
        ghost.message.contains("not a container"),
        "{}",
        ghost.message
    );
}

// --- the binary -----------------------------------------------------------

fn bin(args: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_unit-lint"))
        .args(args)
        .output()
        .unwrap()
}

fn out(o: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

/// The self-test fixture, written by the library: one guest `probe-01`
/// whose profile has `curl` but not `werkzeug-das-es-nicht-gibt`, and a
/// guest-check module that calls both.
#[test]
fn self_test_goes_red_and_reports_it_as_success() {
    let o = bin(&["tools", "--self-test"]);
    assert!(o.status.success(), "{}", out(&o));
    assert!(out(&o).contains("self-test"), "{}", out(&o));
}

#[test]
fn fixture_run_exceptions_and_file_scope() {
    let d = Dir::new("cli");
    let (top, sources) = tools::write_self_test_fixture(&d.0).unwrap();
    let top = top.to_str().unwrap();
    let sources = sources.to_str().unwrap();

    let o = bin(&["tools", top, "--sources", sources]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(
        out(&o).contains("probe-01") && out(&o).contains("werkzeug-das-es-nicht-gibt"),
        "{}",
        out(&o)
    );

    let cfg_ok = d.file(
        "ok.toml",
        "[[except]]\nrule = \"tool-missing\"\nunit = \"werkzeug-das-es-nicht-gibt\"\nsystem = \"probe-01\"\nfile = \"pkgs/gast-pruefung/src/gaeste/probe_01.rs\"\nreason = \"fixture\"\n",
    );
    let o = bin(&[
        "tools",
        "--config",
        cfg_ok.to_str().unwrap(),
        top,
        "--sources",
        sources,
    ]);
    assert_eq!(o.status.code(), Some(0), "{}", out(&o));

    let cfg_wrong = d.file(
        "wrong.toml",
        "[[except]]\nrule = \"tool-missing\"\nunit = \"werkzeug-das-es-nicht-gibt\"\nsystem = \"probe-01\"\nfile = \"docs/runbooks/other.md\"\nreason = \"fixture\"\n",
    );
    let o = bin(&[
        "tools",
        "--config",
        cfg_wrong.to_str().unwrap(),
        top,
        "--sources",
        sources,
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", out(&o));
    assert!(out(&o).contains("stale exception"), "{}", out(&o));

    let empty = d.dir("empty-sources");
    let o = bin(&["tools", top, "--sources", empty.to_str().unwrap()]);
    assert_eq!(o.status.code(), Some(2), "{}", out(&o));
    assert!(out(&o).contains("no call sites"), "{}", out(&o));

    let o = bin(&["tools", "--json", top, "--sources", sources]);
    assert!(out(&o).contains("\"file\""), "{}", out(&o));
}
