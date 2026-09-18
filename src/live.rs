//! Checks against the running system: what the unit files cannot tell.
//!
//! Runs on the host, as root, and reaches into containers with
//! `systemctl -M` and through `/proc/<leader>/root/proc`.

use std::cell::RefCell;
use std::collections::HashMap;
use std::process::Command;

use crate::report::Finding;

pub const LIVE_RULES: &[(&str, &str, &str)] = &[
    (
        "orphan-process",
        "a service still runs although its unit is gone",
        "A deploy that removes a unit does not necessarily stop its process — for units \
         that came from a package, `switch-to-configuration` never touches them. systemd \
         then shows `Loaded: not-found` and `Active: active (running)` at the same time. \
         Stop it by hand: `systemctl stop <unit>`.",
    ),
    (
        "not-loadable",
        "a unit systemd refused to load (bad-setting, error)",
        "The live counterpart of `no-exec`: the unit is not failed, it never got that far, \
         and `systemctl --failed` does not list it.",
    ),
    (
        "stale-sandbox",
        "the process holds capabilities its unit no longer grants",
        "A deploy replaced the unit file, but the process was started under the old one \
         and was not restarted. `systemd-analyze security` reads the unit and reports the \
         new sandbox, while the process still runs with the old one. A process can only \
         drop capabilities from its bounding set, never gain them, so a process with MORE \
         than the unit allows was started before the unit changed. Restart it.",
    ),
];

/// Bit i of a capability mask is CAPS[i].
const CAPS: &[&str] = &[
    "cap_chown",
    "cap_dac_override",
    "cap_dac_read_search",
    "cap_fowner",
    "cap_fsetid",
    "cap_kill",
    "cap_setgid",
    "cap_setuid",
    "cap_setpcap",
    "cap_linux_immutable",
    "cap_net_bind_service",
    "cap_net_broadcast",
    "cap_net_admin",
    "cap_net_raw",
    "cap_ipc_lock",
    "cap_ipc_owner",
    "cap_sys_module",
    "cap_sys_rawio",
    "cap_sys_chroot",
    "cap_sys_ptrace",
    "cap_sys_pacct",
    "cap_sys_admin",
    "cap_sys_boot",
    "cap_sys_nice",
    "cap_sys_resource",
    "cap_sys_time",
    "cap_sys_tty_config",
    "cap_mknod",
    "cap_lease",
    "cap_audit_write",
    "cap_audit_control",
    "cap_setfcap",
    "cap_mac_override",
    "cap_mac_admin",
    "cap_syslog",
    "cap_wake_alarm",
    "cap_block_suspend",
    "cap_audit_read",
    "cap_perfmon",
    "cap_bpf",
    "cap_checkpoint_restore",
];

/// `None` when a name is unknown — then the unit is skipped rather than
/// judged against an incomplete mask.
fn caps_mask(names: &str) -> Option<u64> {
    names.split_whitespace().try_fold(0u64, |m, n| {
        CAPS.iter().position(|c| *c == n).map(|i| m | (1 << i))
    })
}

fn caps_names(mask: u64) -> String {
    CAPS.iter()
        .enumerate()
        .filter(|(i, _)| mask & (1 << i) != 0)
        .map(|(_, n)| *n)
        .collect::<Vec<_>>()
        .join(" ")
}

pub trait Host {
    fn machines(&self) -> Result<Vec<String>, String>;
    fn show_units(&self, machine: Option<&str>) -> Result<String, String>;
    fn proc_status(&self, machine: Option<&str>, pid: u32) -> Option<String>;
}

const PROPERTIES: &str = "Id,LoadState,ActiveState,SubState,MainPID,CapabilityBoundingSet";

/// `systemctl show` blocks, one per unit. Parsed by NAME, never by position:
/// with several `-p`, systemd prints in its own order, not in the one asked
/// for.
pub fn parse_show(text: &str) -> Vec<HashMap<String, String>> {
    text.split("\n\n")
        .map(|block| {
            block
                .lines()
                .filter_map(|l| l.split_once('='))
                .map(|(k, v)| (k.to_owned(), v.to_owned()))
                .collect::<HashMap<_, _>>()
        })
        .filter(|m| m.contains_key("Id"))
        .collect()
}

fn cap_bnd(status: &str) -> Option<u64> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("CapBnd:"))
        .and_then(|v| u64::from_str_radix(v.trim(), 16).ok())
}

pub fn check_system(
    host: &dyn Host,
    machine: Option<&str>,
) -> Result<(usize, Vec<Finding>), String> {
    let system = machine.unwrap_or("host").to_owned();
    let units = parse_show(&host.show_units(machine)?);
    let mut out = Vec::new();
    let f = |rule, unit: &str, message: String| Finding {
        rule,
        system: system.clone(),
        unit: unit.to_owned(),
        message,
    };
    for u in &units {
        let get = |k: &str| u.get(k).map(String::as_str).unwrap_or("");
        let id = get("Id");
        let load = get("LoadState");
        let active = get("ActiveState");
        let pid: u32 = get("MainPID").parse().unwrap_or(0);

        if matches!(load, "bad-setting" | "error") {
            out.push(f("not-loadable", id, format!("LoadState={load}")));
        }
        if load == "not-found"
            && id.ends_with(".service")
            && matches!(active, "active" | "reloading" | "deactivating")
            && (pid > 0 || get("SubState") == "running")
        {
            out.push(f(
                "orphan-process",
                id,
                format!(
                    "unit file is gone, process {pid} is {active} ({})",
                    get("SubState")
                ),
            ));
        }
        if id.ends_with(".service") && active == "active" && pid > 0 && load == "loaded" {
            let (Some(unit_mask), Some(status)) = (
                caps_mask(get("CapabilityBoundingSet")),
                host.proc_status(machine, pid),
            ) else {
                continue;
            };
            let Some(proc_mask) = cap_bnd(&status) else {
                continue;
            };
            let extra = proc_mask & !unit_mask;
            if extra != 0 {
                out.push(f(
                    "stale-sandbox",
                    id,
                    format!(
                        "process {pid} still holds {} — restart it",
                        caps_names(extra)
                    ),
                ));
            }
        }
    }
    Ok((units.len(), out))
}

pub struct RealHost {
    leaders: RefCell<HashMap<String, Option<u32>>>,
}

impl RealHost {
    pub fn new() -> Self {
        RealHost {
            leaders: RefCell::new(HashMap::new()),
        }
    }

    fn leader(&self, machine: &str) -> Option<u32> {
        *self
            .leaders
            .borrow_mut()
            .entry(machine.to_owned())
            .or_insert_with(|| {
                let out = run("machinectl", &["show", machine, "-p", "Leader"]).ok()?;
                out.lines()
                    .find_map(|l| l.strip_prefix("Leader="))
                    .and_then(|v| v.trim().parse().ok())
            })
    }
}

impl Default for RealHost {
    fn default() -> Self {
        Self::new()
    }
}

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

impl Host for RealHost {
    fn machines(&self) -> Result<Vec<String>, String> {
        Ok(run("machinectl", &["list", "--no-legend", "--no-pager"])?
            .lines()
            .filter_map(|l| l.split_whitespace().next())
            .map(str::to_owned)
            .collect())
    }

    fn show_units(&self, machine: Option<&str>) -> Result<String, String> {
        let mut args = vec!["show", "--all", "--no-pager", "--property", PROPERTIES, "*"];
        if let Some(m) = machine {
            args.splice(0..0, ["-M", m]);
        }
        run("systemctl", &args)
    }

    fn proc_status(&self, machine: Option<&str>, pid: u32) -> Option<String> {
        let path = match machine {
            None => format!("/proc/{pid}/status"),
            Some(m) => format!("/proc/{}/root/proc/{pid}/status", self.leader(m)?),
        };
        std::fs::read_to_string(path).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(&'static str, HashMap<u32, &'static str>);

    impl Host for Fake {
        fn machines(&self) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn show_units(&self, _: Option<&str>) -> Result<String, String> {
            Ok(self.0.to_owned())
        }
        fn proc_status(&self, _: Option<&str>, pid: u32) -> Option<String> {
            self.1.get(&pid).map(|s| (*s).to_owned())
        }
    }

    #[test]
    fn masks() {
        assert_eq!(caps_mask("cap_chown cap_kill"), Some(0b100001));
        assert_eq!(caps_mask("cap_chown cap_future"), None);
        assert_eq!(caps_mask(""), Some(0));
        assert_eq!(caps_names(0b100001), "cap_chown cap_kill");
    }

    #[test]
    fn live_rules() {
        let show = "\
Id=auditd.service
LoadState=not-found
ActiveState=active
SubState=running
MainPID=100

Id=prosody.service
MainPID=200
CapabilityBoundingSet=cap_net_bind_service
LoadState=loaded
ActiveState=active
SubState=running

Id=shrinks-itself.service
LoadState=loaded
ActiveState=active
MainPID=300
CapabilityBoundingSet=cap_chown cap_kill

Id=broken.service
LoadState=bad-setting
ActiveState=inactive
MainPID=0

Id=old.service
LoadState=not-found
ActiveState=inactive
MainPID=0
";
        let procs = HashMap::from([
            (200, "Name:\tprosody\nCapBnd:\t0000000000000401\n"),
            (300, "CapBnd:\t0000000000000001\n"),
        ]);
        let (n, found) = check_system(&Fake(show, procs), None).unwrap();
        assert_eq!(n, 5);
        let got: Vec<_> = found.iter().map(|f| (f.rule, f.unit.as_str())).collect();
        assert_eq!(
            got,
            [
                ("orphan-process", "auditd.service"),
                ("stale-sandbox", "prosody.service"),
                ("not-loadable", "broken.service"),
            ]
        );
        assert!(!found[1].message.contains("cap_net_bind_service"));
        assert!(found[1].message.contains("cap_chown"));
    }
}
