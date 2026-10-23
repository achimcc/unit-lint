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
        ]
    );
    assert!(findings.iter().all(|f| f.rule == tools::TOOL_RULE));
    let socat = findings.iter().find(|f| f.unit == "socat").unwrap();
    assert!(
        socat.message.contains("a.rs:5") && socat.message.contains("x.md:3"),
        "{}",
        socat.message
    );
    assert!(socat.message.contains("environment.systemPackages"));
    let ghost = findings.iter().find(|f| f.system == "ghost-01").unwrap();
    assert!(
        ghost.message.contains("not a container"),
        "{}",
        ghost.message
    );
}
