//! `timers`: every timer on the running host and in its containers, as
//! Prometheus metrics.
//!
//! A timer that stopped firing, a timer service that ends with exit 0 before
//! it did its second job, a service that sits in `inactive (dead)` after a
//! clean exit, a oneshot with `RemainAfterExit=yes` that its timer can never
//! start again — none of them is `failed`, so `systemctl --failed` is empty
//! and nothing turns red. Written to a node_exporter textfile every minute,
//! `LastTriggerUSec` and friends become series an alarm can watch:
//! `time() - unit_lint_timer_last_trigger_seconds > 2 * interval`.
//!
//! Two sources, both asked per machine (`systemctl -M`): `list-timers
//! --all --output=json` enumerates the timers and gives `last` and `next` as
//! realtime microseconds (it folds monotonic timers in, which `show`'s
//! `NextElapseUSecRealtime` does not), and `show --timestamp=unix -p …` gives
//! states, results and the service's exit. `show` is parsed by NAME, never by
//! position: with several `-p`, systemd prints in its own order.
//!
//! Everything read here comes from the machines, and a unit name from a
//! guest is data. It is escaped on the way into a label and never becomes a
//! path or a command.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use serde::Deserialize;

use crate::live::parse_show;

/// What `systemctl list-timers --output=json` prints per timer. `next` is
/// `null` for a timer that will not elapse again (`OnBootSec=` after boot,
/// `OnUnitActiveSec=` on a service that stays active); `last` is 0 when it
/// never fired. Both are realtime microseconds.
#[derive(Debug, Deserialize)]
pub struct ListedTimer {
    pub unit: String,
    #[serde(default)]
    pub activates: String,
    #[serde(default)]
    pub next: Option<u64>,
    #[serde(default)]
    pub last: Option<u64>,
}

pub fn parse_list_timers(json: &str) -> Result<Vec<ListedTimer>, String> {
    serde_json::from_str(json).map_err(|e| format!("list-timers: {e}"))
}

/// `machinectl list --output=json`: the running containers.
pub fn parse_machines(json: &str) -> Result<Vec<String>, String> {
    #[derive(Deserialize)]
    struct Machine {
        machine: String,
        #[serde(default)]
        class: String,
    }
    let ms: Vec<Machine> = serde_json::from_str(json).map_err(|e| format!("machinectl: {e}"))?;
    Ok(ms
        .into_iter()
        .filter(|m| m.class == "container")
        .map(|m| m.machine)
        .collect())
}

/// `@1792216800` (what `--timestamp=unix` prints) as seconds. Empty — a
/// timestamp that never happened — is `None`.
pub fn parse_unix_timestamp(v: &str) -> Option<u64> {
    let v = v.trim().strip_prefix('@')?;
    let secs = v.split_once('.').map_or(v, |(s, _)| s);
    secs.parse().ok()
}

pub const TIMER_PROPERTIES: &str = "Id,LoadState,ActiveState,SubState,Result,Unit";
pub const SERVICE_PROPERTIES: &str = "Id,LoadState,ActiveState,SubState,Result,Type,\
                                      RemainAfterExit,ExecMainExitTimestamp,ExecMainStatus,NRestarts";

pub trait TimerHost {
    fn machines(&self) -> Result<Vec<String>, String>;
    fn list_timers(&self, machine: Option<&str>) -> Result<String, String>;
    /// `systemctl show --timestamp=unix -p PROPERTIES UNITS…`.
    fn show(
        &self,
        machine: Option<&str>,
        properties: &str,
        units: &[&str],
    ) -> Result<String, String>;
}

#[derive(Debug, Clone, PartialEq)]
pub struct ServiceState {
    pub loaded: bool,
    pub active: bool,
    pub result: String,
    pub last_exit: Option<u64>,
    pub exit_status: i64,
    pub restarts: u64,
    pub remain_after_exit: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TimerState {
    pub timer: String,
    pub service: String,
    pub last_trigger: Option<u64>,
    pub next_elapse: Option<u64>,
    pub active: bool,
    pub result: String,
    /// `None` when `show` did not return a block for the service at all.
    pub service_state: Option<ServiceState>,
}

#[derive(Debug)]
pub struct MachineTimers {
    pub machine: String,
    /// `Err` is the reason the machine could not be read — a stopped guest,
    /// a `systemctl -M` that failed. It becomes `scrape_ok 0`, not silence.
    pub timers: Result<Vec<TimerState>, String>,
}

/// Reads one machine. `machine` is `None` for the host.
pub fn collect(host: &dyn TimerHost, machine: Option<&str>) -> Result<Vec<TimerState>, String> {
    let listed = parse_list_timers(&host.list_timers(machine)?)?;
    if listed.is_empty() {
        return Ok(vec![]);
    }
    let timer_names: Vec<&str> = listed.iter().map(|t| t.unit.as_str()).collect();
    let timer_blocks = by_id(&host.show(machine, TIMER_PROPERTIES, &timer_names)?);

    // The service each timer activates: `Unit=` from the timer, falling back
    // to list-timers' `activates`.
    let mut services: BTreeMap<String, String> = BTreeMap::new();
    for t in &listed {
        let from_show = timer_blocks
            .get(t.unit.as_str())
            .and_then(|b| b.get("Unit"))
            .filter(|u| !u.is_empty())
            .cloned();
        services.insert(
            t.unit.clone(),
            from_show.unwrap_or_else(|| t.activates.clone()),
        );
    }
    let service_names: Vec<&str> = {
        let mut v: Vec<&str> = services
            .values()
            .map(String::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    };
    let service_blocks = if service_names.is_empty() {
        HashMap::new()
    } else {
        by_id(&host.show(machine, SERVICE_PROPERTIES, &service_names)?)
    };

    let mut out: Vec<TimerState> = listed
        .iter()
        .map(|t| {
            let tb = timer_blocks.get(t.unit.as_str());
            let get = |k: &str| tb.and_then(|b| b.get(k)).map(String::as_str).unwrap_or("");
            let service = services[&t.unit].clone();
            let service_state = service_blocks.get(service.as_str()).map(|b| {
                let g = |k: &str| b.get(k).map(String::as_str).unwrap_or("");
                ServiceState {
                    loaded: g("LoadState") == "loaded",
                    active: g("ActiveState") == "active",
                    result: or_unknown(g("Result")),
                    last_exit: parse_unix_timestamp(g("ExecMainExitTimestamp")),
                    exit_status: g("ExecMainStatus").parse().unwrap_or(0),
                    restarts: g("NRestarts").parse().unwrap_or(0),
                    remain_after_exit: crate::unit::parse_bool(g("RemainAfterExit"))
                        .unwrap_or(false),
                }
            });
            TimerState {
                timer: t.unit.clone(),
                service,
                last_trigger: t.last.filter(|&l| l > 0).map(|l| l / 1_000_000),
                next_elapse: t.next.filter(|&n| n > 0).map(|n| n / 1_000_000),
                active: get("ActiveState") == "active",
                result: or_unknown(get("Result")),
                service_state,
            }
        })
        .collect();
    out.sort_by(|a, b| a.timer.cmp(&b.timer));
    Ok(out)
}

fn or_unknown(v: &str) -> String {
    if v.is_empty() {
        "unknown".to_owned()
    } else {
        v.to_owned()
    }
}

fn by_id(show: &str) -> HashMap<String, HashMap<String, String>> {
    parse_show(show)
        .into_iter()
        .filter_map(|b| b.get("Id").cloned().map(|id| (id, b)))
        .collect()
}

/// Reads the host (labelled `host_label`) and every machine named.
pub fn collect_all(
    host: &dyn TimerHost,
    host_label: &str,
    machines: &[String],
) -> Vec<MachineTimers> {
    let mut out = vec![MachineTimers {
        machine: host_label.to_owned(),
        timers: collect(host, None),
    }];
    let mut names = machines.to_vec();
    names.sort();
    names.dedup();
    for m in names {
        out.push(MachineTimers {
            timers: collect(host, Some(&m)),
            machine: m,
        });
    }
    out
}

/// Prometheus text exposition. Every value is a gauge; the series exist only
/// for timers that exist, so an alarm on `absent()` sees a vanished timer.
pub fn render(machines: &[MachineTimers]) -> String {
    const HELP: &[(&str, &str)] = &[
        (
            "unit_lint_timers_scrape_ok",
            "1 when the machine's timers could be read, 0 when systemctl failed there.",
        ),
        (
            "unit_lint_timers_seen",
            "Number of timers systemd lists on the machine.",
        ),
        (
            "unit_lint_timer_last_trigger_seconds",
            "Unix time the timer last fired; 0 when it never did.",
        ),
        (
            "unit_lint_timer_next_elapse_seconds",
            "Unix time the timer will fire next; 0 when it will not fire again.",
        ),
        ("unit_lint_timer_active", "1 when the timer unit is active."),
        (
            "unit_lint_timer_result",
            "The timer unit's Result= (success, …); 1 on the current value.",
        ),
        (
            "unit_lint_timer_service_loaded",
            "1 when the service the timer activates is loaded (0: it does not exist).",
        ),
        (
            "unit_lint_timer_service_active",
            "1 when the service the timer activates is active right now.",
        ),
        (
            "unit_lint_timer_service_result",
            "The service's Result= (success, exit-code, timeout, …); 1 on the current value.",
        ),
        (
            "unit_lint_timer_service_last_exit_seconds",
            "Unix time the service's main process last exited; 0 when it never ran.",
        ),
        (
            "unit_lint_timer_service_exit_status",
            "Exit status of the service's main process at its last exit.",
        ),
        (
            "unit_lint_timer_service_restarts",
            "NRestarts= of the service: automatic restarts since it was last started by hand or timer.",
        ),
        (
            "unit_lint_timer_service_remain_after_exit",
            "1 when the service has RemainAfterExit=yes: it stays active after the first run, and the timer's later starts do nothing.",
        ),
    ];
    let mut lines: BTreeMap<&str, Vec<String>> = BTreeMap::new();
    let mut push = |name: &'static str, labels: &str, value: String| {
        lines
            .entry(name)
            .or_default()
            .push(format!("{name}{{{labels}}} {value}"));
    };
    for m in machines {
        let ml = format!("machine=\"{}\"", escape(&m.machine));
        let timers = match &m.timers {
            Ok(t) => t,
            Err(_) => {
                push("unit_lint_timers_scrape_ok", &ml, "0".into());
                continue;
            }
        };
        push("unit_lint_timers_scrape_ok", &ml, "1".into());
        push("unit_lint_timers_seen", &ml, timers.len().to_string());
        for t in timers {
            let l = format!(
                "{ml},timer=\"{}\",service=\"{}\"",
                escape(&t.timer),
                escape(&t.service)
            );
            push(
                "unit_lint_timer_last_trigger_seconds",
                &l,
                t.last_trigger.unwrap_or(0).to_string(),
            );
            push(
                "unit_lint_timer_next_elapse_seconds",
                &l,
                t.next_elapse.unwrap_or(0).to_string(),
            );
            push("unit_lint_timer_active", &l, u8::from(t.active).to_string());
            push(
                "unit_lint_timer_result",
                &format!("{l},result=\"{}\"", escape(&t.result)),
                "1".into(),
            );
            let Some(s) = &t.service_state else {
                continue;
            };
            push(
                "unit_lint_timer_service_loaded",
                &l,
                u8::from(s.loaded).to_string(),
            );
            push(
                "unit_lint_timer_service_active",
                &l,
                u8::from(s.active).to_string(),
            );
            push(
                "unit_lint_timer_service_result",
                &format!("{l},result=\"{}\"", escape(&s.result)),
                "1".into(),
            );
            push(
                "unit_lint_timer_service_last_exit_seconds",
                &l,
                s.last_exit.unwrap_or(0).to_string(),
            );
            push(
                "unit_lint_timer_service_exit_status",
                &l,
                s.exit_status.to_string(),
            );
            push(
                "unit_lint_timer_service_restarts",
                &l,
                s.restarts.to_string(),
            );
            push(
                "unit_lint_timer_service_remain_after_exit",
                &l,
                u8::from(s.remain_after_exit).to_string(),
            );
        }
    }
    let mut out = String::new();
    for (name, help) in HELP {
        let Some(series) = lines.get(name) else {
            continue;
        };
        let _ = writeln!(out, "# HELP {name} {help}");
        let _ = writeln!(out, "# TYPE {name} gauge");
        for s in series {
            out.push_str(s);
            out.push('\n');
        }
    }
    out
}

/// Label value escaping of the exposition format: backslash, double quote,
/// newline. Anything else a unit name carries is left as it is.
pub fn escape(v: &str) -> String {
    let mut s = String::with_capacity(v.len());
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            '\n' => s.push_str("\\n"),
            c => s.push(c),
        }
    }
    s
}

/// Writes `text` to `path` through a temporary file next to it and a rename,
/// so node_exporter never reads a half-written file.
pub fn write_textfile(path: &Path, text: &str) -> Result<(), String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{}: not a file name", path.display()))?;
    let tmp = path.with_file_name(format!(".{name}.{}.tmp", std::process::id()));
    let io = |e: std::io::Error| format!("{}: {e}", path.display());
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        io(e)
    })
}

pub struct RealTimerHost;

fn run(cmd: &str, args: &[&str]) -> Result<String, String> {
    let o = Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{cmd}: {e}"))?;
    if !o.status.success() {
        return Err(format!(
            "{cmd} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&o.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&o.stdout).into_owned())
}

fn systemctl(machine: Option<&str>, args: &[&str]) -> Result<String, String> {
    let mut all = Vec::with_capacity(args.len() + 2);
    if let Some(m) = machine {
        all.extend(["-M", m]);
    }
    all.extend_from_slice(args);
    run("systemctl", &all)
}

impl TimerHost for RealTimerHost {
    fn machines(&self) -> Result<Vec<String>, String> {
        parse_machines(&run(
            "machinectl",
            &["list", "--no-pager", "--output=json"],
        )?)
    }

    fn list_timers(&self, machine: Option<&str>) -> Result<String, String> {
        systemctl(
            machine,
            &["list-timers", "--all", "--no-pager", "--output=json"],
        )
    }

    fn show(
        &self,
        machine: Option<&str>,
        properties: &str,
        units: &[&str],
    ) -> Result<String, String> {
        let mut args = vec!["show", "--no-pager", "--timestamp=unix", "-p", properties];
        args.extend_from_slice(units);
        systemctl(machine, &args)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unix_timestamps() {
        assert_eq!(parse_unix_timestamp("@1792216800"), Some(1792216800));
        assert_eq!(parse_unix_timestamp("@1792216800.250000"), Some(1792216800));
        assert_eq!(parse_unix_timestamp(""), None);
        assert_eq!(parse_unix_timestamp("Fri 2026-10-17 10:00:00 CEST"), None);
        assert_eq!(parse_unix_timestamp("@x"), None);
    }

    #[test]
    fn labels_are_escaped() {
        assert_eq!(escape(r#"a"b\c"#), r#"a\"b\\c"#);
        assert_eq!(escape("x\ny"), "x\\ny");
        assert_eq!(escape("plain-01.timer"), "plain-01.timer");
    }

    #[test]
    fn machines_are_containers_only() {
        let json = r#"[{"machine":"a-01","class":"container","service":"systemd-nspawn"},
                       {"machine":"vm-01","class":"vm","service":"x"},
                       {"machine":"b-01","class":"container"}]"#;
        assert_eq!(parse_machines(json).unwrap(), ["a-01", "b-01"]);
        assert!(parse_machines("not json").is_err());
    }

    #[test]
    fn list_timers_tolerates_nulls() {
        let json = r#"[{"next":null,"left":null,"last":0,"passed":0,"unit":"once.timer","activates":"once.service"},
                       {"next":1792219863088570,"left":1,"last":1792219743085488,"passed":2,"unit":"t.timer","activates":"t.service"}]"#;
        let t = parse_list_timers(json).unwrap();
        assert_eq!(t[0].next, None);
        assert_eq!(t[0].last, Some(0));
        assert_eq!(t[1].next, Some(1792219863088570));
    }

    #[test]
    fn textfile_is_replaced_atomically() {
        let dir = std::env::temp_dir().join(format!("unit-lint-textfile-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("timers.prom");
        write_textfile(&p, "a 1\n").unwrap();
        write_textfile(&p, "a 2\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a 2\n");
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "no temporary file stays behind");
        assert!(write_textfile(&dir.join("missing/x.prom"), "a 1\n").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
