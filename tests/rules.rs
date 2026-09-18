//! Every rule has to be able to fire. Each test builds a small unit
//! directory with one broken unit and one healthy twin, and expects exactly
//! the broken one.

use std::fs;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use unit_lint::rules;
use unit_lint::unit;

struct Dir(PathBuf);

impl Dir {
    fn new(name: &str) -> Self {
        let p = std::env::temp_dir().join(format!("unit-lint-{}-{name}", std::process::id()));
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

fn findings(d: &Dir, container: bool, rule: &str) -> Vec<String> {
    let sys = unit::load("t", &d.0, container).unwrap();
    rules::check(&sys, &[rule.to_owned()])
        .into_iter()
        .map(|f| f.unit)
        .collect()
}

#[test]
fn no_exec() {
    let d = Dir::new("no-exec");
    d.file(
        "empty.service",
        "[Unit]\nDescription=x\n[Service]\nEnvironment=A=1\n",
    );
    d.file("fine.service", "[Service]\nExecStart=/bin/true\n");
    d.file(
        "stop-only.service",
        "[Service]\nType=oneshot\nExecStop=/bin/true\n",
    );
    d.file(
        "reset.service",
        "[Service]\nExecStart=/bin/true\nExecStart=\n",
    );
    d.link("masked.service", Path::new("/dev/null"));
    // A drop-in-only unit is someone else's business (dropin-without-unit).
    d.file("pkg.service.d/overrides.conf", "[Service]\nNice=5\n");
    assert_eq!(
        findings(&d, false, "no-exec"),
        ["empty.service", "reset.service"]
    );
}

#[test]
fn dropin_without_unit() {
    let d = Dir::new("dropin");
    d.file("ghost.service.d/overrides.conf", "[Service]\nNice=5\n");
    d.file("real.service", "[Service]\nExecStart=/bin/true\n");
    d.file("real.service.d/overrides.conf", "[Service]\nNice=5\n");
    d.file("tpl@.service", "[Service]\nExecStart=/bin/true\n");
    d.file("tpl@x.service.d/overrides.conf", "[Service]\nNice=5\n");
    d.file(
        "systemd-makefs@.service.d/overrides.conf",
        "[Unit]\nX-A=1\n",
    );
    assert_eq!(
        findings(&d, false, "dropin-without-unit"),
        ["ghost.service"]
    );
}

#[test]
fn timer_never_reruns() {
    let d = Dir::new("rerun");
    d.file(
        "sticky.service",
        "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n",
    );
    d.file("sticky.timer", "[Timer]\nOnUnitActiveSec=15min\n");
    d.file(
        "once.service",
        "[Service]\nType=oneshot\nRemainAfterExit=true\nExecStart=/bin/true\n",
    );
    d.file("once.timer", "[Timer]\nOnBootSec=5min\n");
    d.file(
        "fresh.service",
        "[Service]\nType=oneshot\nExecStart=/bin/true\n",
    );
    d.file("fresh.timer", "[Timer]\nOnCalendar=daily\n");
    d.file(
        "x.service",
        "[Service]\nType=oneshot\nRemainAfterExit=yes\nExecStart=/bin/true\n",
    );
    d.file("other.timer", "[Timer]\nOnCalendar=daily\nUnit=x.service\n");
    assert_eq!(
        findings(&d, false, "timer-never-reruns"),
        ["x.service", "sticky.service"]
    );
}

#[test]
fn timer_unit_missing() {
    let d = Dir::new("timer-missing");
    d.file("lonely.timer", "[Timer]\nOnCalendar=daily\n");
    d.file("ok.timer", "[Timer]\nOnCalendar=daily\n");
    d.file("ok.service", "[Service]\nExecStart=/bin/true\n");
    assert_eq!(findings(&d, false, "timer-unit-missing"), ["lonely.timer"]);
}

#[test]
fn on_failure_missing() {
    let d = Dir::new("on-failure");
    d.file("mail@.service", "[Service]\nExecStart=/bin/true\n");
    d.file(
        "a.service",
        "[Unit]\nOnFailure=mail@%n.service\n[Service]\nExecStart=/bin/true\n",
    );
    d.file(
        "b.service",
        "[Unit]\nOnFailure=nowhere.service\n[Service]\nExecStart=/bin/true\n",
    );
    d.file("c.target", "[Unit]\nOnFailure=emergency.target\n");
    assert_eq!(
        findings(&d, false, "on-failure-missing"),
        ["b.service", "c.target"]
    );
}

#[test]
fn oneshot_restart_forever() {
    let d = Dir::new("restart");
    let body = "[Service]\nType=oneshot\nExecStart=/bin/false\nRestart=on-failure\n";
    d.file("slow.service", &format!("{body}RestartSec=30s\n"));
    d.file("fast.service", body); // 100ms default: trips at 5 in 10s
    d.file(
        "bounded.service",
        &format!("[Unit]\nStartLimitIntervalSec=10min\nStartLimitBurst=5\n{body}RestartSec=30s\n"),
    );
    d.file(
        "unlimited.service",
        &format!("[Unit]\nStartLimitIntervalSec=0\n{body}"),
    );
    d.file(
        "simple.service",
        "[Service]\nExecStart=/bin/false\nRestart=always\nRestartSec=30s\n",
    );
    assert_eq!(
        findings(&d, false, "oneshot-restart-forever"),
        ["slow.service", "unlimited.service"]
    );
}

#[test]
fn oneshot_waits_in_boot() {
    let d = Dir::new("waits");
    let wait = d.file(
        "scripts/wait",
        "#!/bin/sh\nuntil [ -e /x ]; do\n  sleep 1\ndone\n",
    );
    let quick = d.file("scripts/quick", "#!/bin/sh\necho hi\n");
    let reader = d.file(
        "scripts/reader",
        "#!/bin/sh\nwhile read l; do echo $l; done\n",
    );
    let sys = d.0.join("sys");
    let unit = |name: &str, script: &Path, extra: &str| {
        d.file(
            &format!("sys/{name}"),
            &format!(
                "[Service]\nType=oneshot\nExecStart={}\n{extra}",
                script.display()
            ),
        );
        d.link(
            &format!("sys/multi-user.target.wants/{name}"),
            &sys.join(name),
        );
    };
    unit("waits.service", &wait, "");
    unit("bounded.service", &wait, "TimeoutStartSec=5min\n");
    unit("quick.service", &quick, "");
    unit("reader.service", &reader, "");
    // Not pulled into boot:
    d.file(
        "sys/offboot.service",
        &format!("[Service]\nType=oneshot\nExecStart={}\n", wait.display()),
    );
    let run = |container| {
        let s = unit::load("t", &sys, container).unwrap();
        rules::check(&s, &["oneshot-waits-in-boot".into()])
            .into_iter()
            .map(|f| f.unit)
            .collect::<Vec<_>>()
    };
    assert_eq!(run(true), ["waits.service"]);
    assert!(run(false).is_empty(), "hosts are not checked");
}

#[test]
fn podman_mount_namespace() {
    let d = Dir::new("podman");
    d.file(
        "podman-app.service",
        "[Service]\nExecStart=/bin/podman-app-start\nProtectHome=yes\nProtectKernelLogs=true\n",
    );
    d.file(
        "podman-ok.service",
        "[Service]\nExecStart=/bin/podman-ok-start\nProtectHome=no\nNoNewPrivileges=yes\n",
    );
    d.file(
        "by-exec.service",
        "[Service]\nExecStart=/nix/store/x-podman/bin/podman run x\nPrivateTmp=yes\n",
    );
    d.file(
        "plain.service",
        "[Service]\nExecStart=/bin/true\nProtectHome=yes\n",
    );
    assert_eq!(
        findings(&d, false, "podman-mount-namespace"),
        ["by-exec.service", "podman-app.service"]
    );
}

#[test]
fn toplevel_with_containers() {
    let d = Dir::new("toplevel");
    d.file("host/etc/systemd/system/a.service", "[Service]\n");
    d.file("guest/etc/systemd/system/b.service", "[Service]\n");
    d.file(
        "host/etc/nixos-containers/g1.conf",
        &format!("SYSTEM_PATH={}\n", d.0.join("guest").display()),
    );
    let systems = unit::load_toplevel("host", &d.0.join("host")).unwrap();
    let names: Vec<_> = systems
        .iter()
        .map(|s| (s.name.as_str(), s.container))
        .collect();
    assert_eq!(names, [("host", false), ("g1", true)]);
}
