//! `timers` against recorded `systemctl` output: the host and one container
//! of a real homelab (systemd 260, 2026-10-17), plus a machine that cannot be
//! read. The rendered metrics are compared to `tests/fixtures/timers/
//! expected.prom`; `promtool check metrics` accepts that file (checked by
//! hand when it changes — `UPDATE_EXPECTED=1 cargo test` rewrites it).

use std::collections::HashMap;
use std::path::PathBuf;

use unit_lint::timers::{self, TimerHost};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/timers")
}

fn fixture(name: &str) -> String {
    let p = fixtures().join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// Answers from the recordings; `down-01` fails like a stopped container.
struct Recorded {
    /// (machine, properties) → recorded `show` output; the units asked for
    /// are checked against the recording's blocks.
    shows: HashMap<(String, &'static str), String>,
}

impl Recorded {
    fn new() -> Self {
        let mut shows = HashMap::new();
        for m in ["host", "infra-01"] {
            shows.insert(
                (m.to_owned(), timers::TIMER_PROPERTIES),
                fixture(&format!("{m}-show-timers.txt")),
            );
            shows.insert(
                (m.to_owned(), timers::SERVICE_PROPERTIES),
                fixture(&format!("{m}-show-services.txt")),
            );
        }
        Recorded { shows }
    }
}

impl TimerHost for Recorded {
    fn machines(&self) -> Result<Vec<String>, String> {
        Ok(vec!["infra-01".into(), "down-01".into()])
    }

    fn list_timers(&self, machine: Option<&str>) -> Result<String, String> {
        match machine {
            None => Ok(fixture("host-list-timers.json")),
            Some("infra-01") => Ok(fixture("infra-01-list-timers.json")),
            Some(m) => Err(format!("systemctl -M {m}: Failed to connect to bus")),
        }
    }

    fn show(
        &self,
        machine: Option<&str>,
        properties: &str,
        units: &[&str],
    ) -> Result<String, String> {
        let m = machine.unwrap_or("host").to_owned();
        let text = self
            .shows
            .get(&(m.clone(), properties))
            .unwrap_or_else(|| panic!("unexpected show on {m} for {properties}"));
        // Every unit asked for is one the recording knows — the recording was
        // made with exactly these names.
        for u in units {
            assert!(
                text.contains(&format!("Id={u}\n")),
                "{m}: {u} was not in the recording"
            );
        }
        Ok(text.clone())
    }
}

fn find<'a>(ms: &'a [timers::MachineTimers], machine: &str, timer: &str) -> &'a timers::TimerState {
    ms.iter()
        .find(|m| m.machine == machine)
        .unwrap_or_else(|| panic!("machine {machine}"))
        .timers
        .as_ref()
        .unwrap()
        .iter()
        .find(|t| t.timer == timer)
        .unwrap_or_else(|| panic!("{machine}: {timer}"))
}

#[test]
fn recorded_machines() {
    let host = Recorded::new();
    let machines = host.machines().unwrap();
    let read = timers::collect_all(&host, "wirt", &machines);

    let summary: Vec<(&str, Result<usize, &str>)> = read
        .iter()
        .map(|m| {
            (
                m.machine.as_str(),
                m.timers.as_ref().map(Vec::len).map_err(String::as_str),
            )
        })
        .collect();
    assert_eq!(
        summary,
        [
            ("wirt", Ok(47)),
            (
                "down-01",
                Err("systemctl -M down-01: Failed to connect to bus")
            ),
            ("infra-01", Ok(5)),
        ]
    );

    // An hourly timer: fired at the full hour, fires at the next.
    let logrotate = find(&read, "infra-01", "logrotate.timer");
    assert_eq!(logrotate.service, "logrotate.service");
    assert_eq!(logrotate.last_trigger, Some(1792216813));
    assert_eq!(logrotate.next_elapse, Some(1792220400));
    assert!(logrotate.active);
    assert_eq!(logrotate.result, "success");
    let svc = logrotate.service_state.as_ref().unwrap();
    assert_eq!(svc.last_exit, Some(1792216813));
    assert!(svc.loaded && !svc.active && !svc.remain_after_exit);
    assert_eq!((svc.exit_status, svc.restarts), (0, 0));

    // Monotonic timers (OnUnitActiveSec=) have no NextElapseUSecRealtime in
    // `show`; list-timers folds them in. ddclient on infra-01 is one.
    let ddclient = find(&read, "infra-01", "ddclient.timer");
    assert!(
        ddclient.next_elapse.is_some(),
        "next from list-timers, not show"
    );

    // Never fired, never will until something enables it: both zero.
    let fstrim = find(&read, "infra-01", "fstrim.timer");
    assert_eq!((fstrim.last_trigger, fstrim.next_elapse), (None, None));
    assert!(!fstrim.active);
    assert_eq!(fstrim.service_state.as_ref().unwrap().last_exit, None);

    // The trap itself: a timer on a service with RemainAfterExit=yes. The
    // service stays `active (exited)`, the timer shows `running`, and the
    // next start will do nothing. On the recorded host there is exactly one.
    let sticky: Vec<&str> = read[0]
        .timers
        .as_ref()
        .unwrap()
        .iter()
        .filter(|t| {
            t.service_state
                .as_ref()
                .is_some_and(|s| s.remain_after_exit)
        })
        .map(|t| t.timer.as_str())
        .collect();
    assert_eq!(sticky, ["kernel-modulsperre.timer"]);
    assert!(
        find(&read, "wirt", "kernel-modulsperre.timer")
            .next_elapse
            .is_none()
    );

    // An OnBootSec= timer that elapsed: still active, nothing ahead.
    let boot = find(&read, "wirt", "tetragon-boot-neustart.timer");
    assert!(boot.active && boot.next_elapse.is_none() && boot.last_trigger.is_some());
}

#[test]
fn rendered_metrics_match_the_expected_file() {
    let host = Recorded::new();
    let machines = host.machines().unwrap();
    let got = timers::render(&timers::collect_all(&host, "wirt", &machines));
    let path = fixtures().join("expected.prom");
    if std::env::var_os("UPDATE_EXPECTED").is_some() {
        std::fs::write(&path, &got).unwrap();
    }
    let want = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    if got != want {
        let first = got
            .lines()
            .zip(want.lines())
            .position(|(g, w)| g != w)
            .map(|i| format!("first difference at line {}", i + 1))
            .unwrap_or_else(|| "one is a prefix of the other".into());
        panic!("rendered metrics differ from expected.prom: {first}\n{got}");
    }

    // The failed machine is a series, not an absence.
    assert!(got.contains("unit_lint_timers_scrape_ok{machine=\"down-01\"} 0\n"));
    assert!(got.contains("unit_lint_timers_scrape_ok{machine=\"infra-01\"} 1\n"));
    assert!(got.contains("unit_lint_timers_seen{machine=\"wirt\"} 47\n"));
    assert!(got.contains(
        "unit_lint_timer_service_remain_after_exit{machine=\"wirt\",timer=\"kernel-modulsperre.timer\",service=\"kernel-modulsperre.service\"} 1\n"
    ));
    // Every HELP has its TYPE, every metric name appears in HELP exactly once.
    let helps = got.lines().filter(|l| l.starts_with("# HELP ")).count();
    let types = got.lines().filter(|l| l.starts_with("# TYPE ")).count();
    assert_eq!(helps, types);
    assert_eq!(helps, 13);
}

#[test]
fn a_timer_whose_service_is_gone_still_has_its_own_series() {
    struct Lonely;
    impl TimerHost for Lonely {
        fn machines(&self) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn list_timers(&self, _: Option<&str>) -> Result<String, String> {
            Ok(r#"[{"next":null,"left":null,"last":0,"passed":0,"unit":"lonely.timer","activates":"gone.service"}]"#.into())
        }
        fn show(&self, _: Option<&str>, props: &str, units: &[&str]) -> Result<String, String> {
            Ok(if props == timers::TIMER_PROPERTIES {
                assert_eq!(units, ["lonely.timer"]);
                "Id=lonely.timer\nLoadState=loaded\nActiveState=active\nSubState=waiting\nUnit=\nResult=success\n".into()
            } else {
                assert_eq!(units, ["gone.service"]);
                "Id=gone.service\nLoadState=not-found\nActiveState=inactive\nSubState=dead\nResult=exit-code\nNRestarts=0\nExecMainStatus=203\nExecMainExitTimestamp=\nRemainAfterExit=no\n".into()
            })
        }
    }
    let read = timers::collect_all(&Lonely, "host", &[]);
    let t = find(&read, "host", "lonely.timer");
    assert_eq!(
        t.service, "gone.service",
        "falls back to list-timers' activates"
    );
    assert_eq!(t.last_trigger, None, "last=0 is never");
    let s = t.service_state.as_ref().unwrap();
    assert!(!s.loaded);
    let text = timers::render(&read);
    assert!(text.contains(
        "unit_lint_timer_service_loaded{machine=\"host\",timer=\"lonely.timer\",service=\"gone.service\"} 0\n"
    ));
    assert!(text.contains(
        "unit_lint_timer_last_trigger_seconds{machine=\"host\",timer=\"lonely.timer\",service=\"gone.service\"} 0\n"
    ));
    assert!(text.contains(
        "unit_lint_timer_service_result{machine=\"host\",timer=\"lonely.timer\",service=\"gone.service\",result=\"exit-code\"} 1\n"
    ));
    assert!(text.contains(
        "unit_lint_timer_service_exit_status{machine=\"host\",timer=\"lonely.timer\",service=\"gone.service\"} 203\n"
    ));
}

#[test]
fn an_empty_machine_is_fine() {
    struct Empty;
    impl TimerHost for Empty {
        fn machines(&self) -> Result<Vec<String>, String> {
            Ok(vec![])
        }
        fn list_timers(&self, _: Option<&str>) -> Result<String, String> {
            Ok("[]".into())
        }
        fn show(&self, _: Option<&str>, _: &str, _: &[&str]) -> Result<String, String> {
            panic!("show must not be called for an empty list")
        }
    }
    let read = timers::collect_all(&Empty, "host", &[]);
    let text = timers::render(&read);
    assert_eq!(
        text,
        "# HELP unit_lint_timers_scrape_ok 1 when the machine's timers could be read, 0 when systemctl failed there.\n\
         # TYPE unit_lint_timers_scrape_ok gauge\n\
         unit_lint_timers_scrape_ok{machine=\"host\"} 1\n\
         # HELP unit_lint_timers_seen Number of timers systemd lists on the machine.\n\
         # TYPE unit_lint_timers_seen gauge\n\
         unit_lint_timers_seen{machine=\"host\"} 0\n"
    );
}
