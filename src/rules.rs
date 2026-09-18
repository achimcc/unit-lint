//! The static rules. Each one describes a unit that loads — or at least
//! builds — without complaint and still does not do what it says.

use std::fs;

use crate::report::Finding;
use crate::unit::{System, Unit, parse_bool, parse_timespan};

pub struct Rule {
    pub id: &'static str,
    pub summary: &'static str,
    pub explain: &'static str,
    check: fn(&System, &mut Vec<Finding>),
}

pub const RULES: &[Rule] = &[
    Rule {
        id: "no-exec",
        summary: "service without ExecStart=, ExecStop= or SuccessAction=",
        explain: "systemd refuses to load such a service (LoadState=bad-setting). It is not \
                  `failed`, so `systemctl --failed` stays empty, and an OnFailure= alarm hanging \
                  on it can never fire. In NixOS the usual cause is `lib.mkIf` one level too \
                  deep: `systemd.services.foo.serviceConfig = lib.mkIf cond {...}` removes the \
                  content but keeps the unit.",
        check: no_exec,
    },
    Rule {
        id: "dropin-without-unit",
        summary: "drop-in directory for a unit that does not exist",
        explain: "NixOS writes settings for a unit that a package is expected to ship as a \
                  drop-in only. If no package ships it, the unit is `not-found` and the settings \
                  go nowhere.",
        check: dropin_without_unit,
    },
    Rule {
        id: "timer-never-reruns",
        summary: "repeating timer on a service with RemainAfterExit=yes",
        explain: "A service with RemainAfterExit=yes stays `active (exited)` after its first \
                  run. Starting an active unit is a no-op, so the timer fires and nothing runs \
                  — ever again, until the unit is stopped or the machine reboots. \
                  OnUnitInactiveSec= never even fires.",
        check: timer_never_reruns,
    },
    Rule {
        id: "timer-unit-missing",
        summary: "timer whose unit does not exist",
        explain: "The timer is active and elapses; the start job fails with `Unit not found`, \
                  and nothing turns red.",
        check: timer_unit_missing,
    },
    Rule {
        id: "on-failure-missing",
        summary: "OnFailure=/OnSuccess= names a unit that does not exist",
        explain: "An alarm on a unit that is not there is no alarm. The failure is reported \
                  once in the journal, when it happens, and nowhere else.",
        check: on_failure_missing,
    },
    Rule {
        id: "oneshot-restart-forever",
        summary: "Type=oneshot with Restart= whose start limit can never trip",
        explain: "A oneshot's start job lasts until it succeeds. With a restart delay so long \
                  that the start limit (default 5 in 10s) is never reached, a failing oneshot \
                  restarts forever — and everything waiting for it waits forever: a container \
                  that never finishes booting, a deploy that never finishes switching.",
        check: oneshot_restart_forever,
    },
    Rule {
        id: "oneshot-waits-in-boot",
        summary: "container: Type=oneshot with a wait loop, pulled into boot, without a start timeout",
        explain: "A oneshot has no start timeout by default (TimeoutStartSec=infinity), and \
                  multi-user.target waits for it. In a container, a script that polls for \
                  something (`until ...; do sleep ...`) holds the whole boot: the host's \
                  `container@` unit stays `activating`, and whatever started it — a deploy — \
                  fails or hangs. Use Type=simple for work that runs after a service, or set \
                  TimeoutStartSec=. Checked in containers only: on a host, boot-time waits \
                  (acme's renewal lock, for one) are ordinary.",
        check: oneshot_waits_in_boot,
    },
    Rule {
        id: "podman-mount-namespace",
        summary: "podman unit with a sandbox option that creates a mount namespace",
        explain: "podman binds each container's network namespace to /run/netns/netns-<id> \
                  from inside ExecStart's mount namespace; it does not propagate out. ExecStop \
                  gets a fresh namespace, finds an empty file there, netavark fails with \
                  `setns: Invalid argument`, and the DNAT rule of the dead container stays. \
                  iptables uses the first match, so after a restart the published port points \
                  at an address nobody has — a 502 behind a proxy, while every unit is green.",
        check: podman_mount_namespace,
    },
];

fn finding(sys: &System, unit: &str, rule: &'static str, message: String) -> Finding {
    Finding {
        rule,
        system: sys.name.clone(),
        unit: unit.to_owned(),
        message,
    }
}

fn services(sys: &System) -> impl Iterator<Item = &Unit> {
    sys.units
        .values()
        .filter(|u| u.kind() == "service" && !u.masked && !u.name.contains("@."))
}

fn no_exec(sys: &System, out: &mut Vec<Finding>) {
    for u in sys.units.values() {
        if u.kind() != "service" || u.masked || u.file.is_none() {
            continue;
        }
        if !u.has("Service", "ExecStart")
            && !u.has("Service", "ExecStop")
            && u.last("Unit", "SuccessAction").is_none()
        {
            out.push(finding(
                sys,
                &u.name,
                "no-exec",
                "no ExecStart=, ExecStop= or SuccessAction= — systemd will refuse to load it"
                    .into(),
            ));
        }
    }
}

/// Units that systemd's generators write at boot into /run/systemd/generator
/// (fstab: makefs, mkswap, growfs; ssh: the vsock and local-socket sshd). A
/// drop-in for them is fine although no file exists at build time.
const GENERATED: &[&str] = &[
    "systemd-makefs@",
    "systemd-mkswap@",
    "systemd-growfs@",
    "systemd-growfs-root",
    "sshd-vsock",
    "sshd-unix-local",
];

fn dropin_without_unit(sys: &System, out: &mut Vec<Finding>) {
    for u in sys.units.values() {
        if u.file.is_some() || u.masked || u.dropins.is_empty() {
            continue;
        }
        // An instance lives off its template.
        if Unit::template_name(&u.name).is_some_and(|t| sys.exists(&t)) {
            continue;
        }
        if GENERATED.iter().any(|g| u.name.starts_with(g)) {
            continue;
        }
        // Templates are instantiated at runtime; a drop-in for `foo@.service`
        // alone says nothing about whether `foo@.service` exists — unless it
        // doesn't, which is the case we look for.
        out.push(finding(
            sys,
            &u.name,
            "dropin-without-unit",
            format!(
                "{} drop-in(s), but no unit file — the unit will be not-found",
                u.dropins.len()
            ),
        ));
    }
}

fn timer_target(t: &Unit) -> String {
    t.last("Timer", "Unit")
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{}.service", t.name.trim_end_matches(".timer")))
}

fn timers(sys: &System) -> impl Iterator<Item = &Unit> {
    sys.units
        .values()
        .filter(|u| u.kind() == "timer" && !u.masked && u.file.is_some() && !u.name.contains("@."))
}

fn timer_never_reruns(sys: &System, out: &mut Vec<Finding>) {
    for t in timers(sys) {
        let repeating: Vec<&str> = ["OnCalendar", "OnUnitActiveSec", "OnUnitInactiveSec"]
            .into_iter()
            .filter(|k| t.has("Timer", k))
            .collect();
        if repeating.is_empty() {
            continue;
        }
        let target = timer_target(t);
        let Some(s) = sys.lookup(&target) else {
            continue;
        };
        if s.last("Service", "RemainAfterExit")
            .and_then(parse_bool)
            .unwrap_or(false)
        {
            out.push(finding(
                sys,
                &target,
                "timer-never-reruns",
                format!(
                    "RemainAfterExit=yes, but {} repeats it ({}) — it runs once, then never again",
                    t.name,
                    repeating.join(", ")
                ),
            ));
        }
    }
}

fn timer_unit_missing(sys: &System, out: &mut Vec<Finding>) {
    for t in timers(sys) {
        let target = timer_target(t);
        if !sys.exists(&target) {
            out.push(finding(
                sys,
                &t.name,
                "timer-unit-missing",
                format!("triggers {target}, which does not exist"),
            ));
        }
    }
}

/// Enough of systemd's specifiers to resolve the unit names people write.
fn expand_specifiers(value: &str, unit: &str) -> String {
    let stem = unit.rsplit_once('.').map_or(unit, |(s, _)| s);
    let (prefix, instance) = stem.split_once('@').unwrap_or((stem, ""));
    value
        .replace("%n", unit)
        .replace("%N", stem)
        .replace("%p", prefix)
        .replace("%i", instance)
        .replace("%I", instance)
        .replace("%%", "%")
}

fn on_failure_missing(sys: &System, out: &mut Vec<Finding>) {
    for u in sys.units.values() {
        if u.masked || u.name.contains("@.") || u.file.is_none() {
            continue;
        }
        for key in ["OnFailure", "OnSuccess"] {
            for v in u.list("Unit", key) {
                for name in v.split_whitespace() {
                    let name = expand_specifiers(name, &u.name);
                    if !sys.exists(&name) {
                        out.push(finding(
                            sys,
                            &u.name,
                            "on-failure-missing",
                            format!("{key}={name}, which does not exist"),
                        ));
                    }
                }
            }
        }
    }
}

fn start_limit(u: &Unit) -> (Option<f64>, u32) {
    let interval = u
        .last("Unit", "StartLimitIntervalSec")
        .or_else(|| u.last("Service", "StartLimitInterval"))
        .or_else(|| u.last("Service", "StartLimitIntervalSec"))
        .and_then(parse_timespan)
        .unwrap_or(Some(10.0));
    let burst = u
        .last("Unit", "StartLimitBurst")
        .or_else(|| u.last("Service", "StartLimitBurst"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(5);
    // StartLimitIntervalSec=0 switches the limit off; infinity keeps
    // counting forever, so it trips at the burst.
    (
        if interval == Some(0.0) {
            None
        } else {
            interval.or(Some(f64::INFINITY))
        },
        burst,
    )
}

fn is_oneshot(u: &Unit) -> bool {
    u.last("Service", "Type") == Some("oneshot")
}

fn oneshot_restart_forever(sys: &System, out: &mut Vec<Finding>) {
    for u in services(sys) {
        if !is_oneshot(u) {
            continue;
        }
        let restart = u.last("Service", "Restart").unwrap_or("no");
        if restart == "no" {
            continue;
        }
        let delay = u
            .last("Service", "RestartSec")
            .and_then(parse_timespan)
            .unwrap_or(Some(0.1))
            .unwrap_or(f64::INFINITY);
        let (interval, burst) = start_limit(u);
        let reason = match interval {
            None => "StartLimitIntervalSec=0 switches the limit off".to_owned(),
            Some(i) if burst == 0 => format!("StartLimitBurst=0 within {i}s"),
            Some(i) if delay * f64::from(burst) >= i => {
                format!("{burst} restarts {delay}s apart never fit into {i}s")
            }
            Some(_) => continue,
        };
        out.push(finding(
            sys,
            &u.name,
            "oneshot-restart-forever",
            format!("Restart={restart}, and the start limit cannot trip: {reason}"),
        ));
    }
}

/// The program of an Exec line, without systemd's prefixes (`-@:+!`).
fn exec_path(line: &str) -> &str {
    line.trim_start_matches(['-', '@', ':', '+', '!'])
        .split_whitespace()
        .next()
        .unwrap_or("")
}

fn script_waits(path: &str) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() || meta.len() > 2 * 1024 * 1024 {
        return false;
    }
    let Ok(text) = fs::read_to_string(path) else {
        return false;
    };
    text.lines().any(|l| {
        let l = l.trim_start();
        (l.starts_with("until ") || l.starts_with("while ")) && !l.starts_with("while read")
    }) && text.contains("sleep")
}

fn oneshot_waits_in_boot(sys: &System, out: &mut Vec<Finding>) {
    if !sys.container {
        return;
    }
    for u in services(sys) {
        if !is_oneshot(u) {
            continue;
        }
        let Some(target) = ["multi-user.target", "default.target", "sysinit.target"]
            .into_iter()
            .find(|t| sys.pulled_in_by(t, &u.name))
        else {
            continue;
        };
        let timeout = u
            .last("Service", "TimeoutStartSec")
            .or_else(|| u.last("Service", "TimeoutSec"))
            .and_then(parse_timespan);
        if matches!(timeout, Some(Some(_))) {
            continue;
        }
        if let Some(script) = u
            .list("Service", "ExecStart")
            .into_iter()
            .map(exec_path)
            .find(|p| script_waits(p))
        {
            out.push(finding(
                sys,
                &u.name,
                "oneshot-waits-in-boot",
                format!(
                    "{target} waits for it, it has no start timeout, and {} polls in a loop",
                    script.rsplit('/').next().unwrap_or(script)
                ),
            ));
        }
    }
}

/// Options that give the unit a mount namespace of its own, with the values
/// that mean "off".
const MOUNT_NS_OPTIONS: &[(&str, &[&str])] = &[
    ("ProtectSystem", &["no", "false"]),
    ("ProtectHome", &["no", "false"]),
    ("PrivateTmp", &["no", "false"]),
    ("PrivateDevices", &["no", "false"]),
    ("PrivateMounts", &["no", "false"]),
    ("ProtectKernelTunables", &["no", "false"]),
    ("ProtectKernelModules", &["no", "false"]),
    ("ProtectKernelLogs", &["no", "false"]),
    ("ProtectControlGroups", &["no", "false"]),
    ("ProtectProc", &["default"]),
    ("ProcSubset", &["all"]),
    ("ReadWritePaths", &[]),
    ("ReadOnlyPaths", &[]),
    ("InaccessiblePaths", &[]),
    ("ExecPaths", &[]),
    ("NoExecPaths", &[]),
    ("TemporaryFileSystem", &[]),
    ("BindPaths", &[]),
    ("BindReadOnlyPaths", &[]),
    ("RootDirectory", &[]),
    ("RootImage", &[]),
    ("MountFlags", &[]),
];

fn runs_podman(u: &Unit) -> bool {
    u.name.starts_with("podman-")
        || u.list("Service", "ExecStart")
            .into_iter()
            .any(|l| exec_path(l).ends_with("/bin/podman"))
}

fn podman_mount_namespace(sys: &System, out: &mut Vec<Finding>) {
    for u in services(sys) {
        if !runs_podman(u) {
            continue;
        }
        let set: Vec<&str> = MOUNT_NS_OPTIONS
            .iter()
            .filter(|(key, off)| {
                u.last("Service", key).is_some_and(|v| {
                    !off.iter().any(|o| o.eq_ignore_ascii_case(v)) && parse_bool(v) != Some(false)
                })
            })
            .map(|(k, _)| *k)
            .collect();
        if !set.is_empty() {
            out.push(finding(
                sys,
                &u.name,
                "podman-mount-namespace",
                format!(
                    "{} — ExecStop will not find the container's netns",
                    set.join(", ")
                ),
            ));
        }
    }
}

pub fn check(sys: &System, only: &[String]) -> Vec<Finding> {
    let mut out = Vec::new();
    for r in RULES {
        if only.is_empty() || only.iter().any(|o| o == r.id) {
            (r.check)(sys, &mut out);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specifiers() {
        assert_eq!(
            expand_specifiers("mail@%n.service", "foo.service"),
            "mail@foo.service.service"
        );
        assert_eq!(
            expand_specifiers("x@%i.service", "a@b.service"),
            "x@b.service"
        );
    }

    #[test]
    fn exec_prefixes() {
        assert_eq!(exec_path("-@+/bin/sh -c x"), "/bin/sh");
    }
}
