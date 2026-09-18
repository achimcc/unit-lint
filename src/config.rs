//! Exceptions: a finding that is deliberate says so, with a reason.
//!
//! ```toml
//! [[except]]
//! rule = "oneshot-waits-in-boot"
//! unit = "grafana-setup.service"
//! system = "obs-01"          # optional; every system when missing
//! reason = "polls at most 30 s, measured 2026-09-18"
//! ```
//!
//! `unit` may end in `*` to cover a family (`podman-*`). An exception that
//! matches nothing is reported, so the list cannot rot quietly.

use serde::Deserialize;

use crate::report::{Finding, Report};

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default, rename = "except")]
    pub exceptions: Vec<Exception>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Exception {
    pub rule: String,
    pub unit: String,
    pub system: Option<String>,
    pub reason: String,
}

impl Exception {
    fn matches(&self, f: &Finding) -> bool {
        self.rule == f.rule
            && self.system.as_ref().is_none_or(|s| *s == f.system)
            && match self.unit.strip_suffix('*') {
                Some(prefix) => f.unit.starts_with(prefix),
                None => self.unit == f.unit,
            }
    }

    fn describe(&self) -> String {
        format!(
            "[{}] {}{}",
            self.rule,
            self.unit,
            self.system
                .as_deref()
                .map(|s| format!(" in {s}"))
                .unwrap_or_default()
        )
    }
}

pub fn parse(text: &str) -> Result<Config, String> {
    let c: Config = toml::from_str(text).map_err(|e| e.to_string())?;
    for e in &c.exceptions {
        if e.reason.trim().is_empty() {
            return Err(format!("exception {} has an empty reason", e.describe()));
        }
    }
    Ok(c)
}

/// Splits findings into reported and excepted. `rules_run` and
/// `systems_seen` limit the staleness check to exceptions this run could
/// have matched.
pub fn apply(
    config: &Config,
    findings: Vec<Finding>,
    rules_run: &[&str],
    systems_seen: &[String],
    systems: usize,
    units: usize,
) -> Report {
    let mut used = vec![false; config.exceptions.len()];
    let mut reported = Vec::new();
    let mut excepted = Vec::new();
    for f in findings {
        match config.exceptions.iter().position(|e| e.matches(&f)) {
            Some(i) => {
                used[i] = true;
                excepted.push((f, config.exceptions[i].reason.clone()));
            }
            None => reported.push(f),
        }
    }
    let stale_exceptions = config
        .exceptions
        .iter()
        .zip(used)
        .filter(|(e, used)| {
            !used
                && rules_run.contains(&e.rule.as_str())
                && e.system.as_ref().is_none_or(|s| systems_seen.contains(s))
        })
        .map(|(e, _)| e.describe())
        .collect();
    Report {
        systems,
        units,
        findings: reported,
        excepted,
        stale_exceptions,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(rule: &'static str, system: &str, unit: &str) -> Finding {
        Finding {
            rule,
            system: system.into(),
            unit: unit.into(),
            message: String::new(),
        }
    }

    #[test]
    fn except_and_stale() {
        let c = parse(
            r#"
            [[except]]
            rule = "no-exec"
            unit = "podman-*"
            reason = "r"
            [[except]]
            rule = "no-exec"
            unit = "gone.service"
            system = "a"
            reason = "r"
            [[except]]
            rule = "no-exec"
            unit = "elsewhere.service"
            system = "b"
            reason = "r"
        "#,
        )
        .unwrap();
        let r = apply(
            &c,
            vec![
                f("no-exec", "a", "podman-x.service"),
                f("no-exec", "a", "y.service"),
            ],
            &["no-exec"],
            &["a".into()],
            1,
            2,
        );
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.excepted.len(), 1);
        // `b` was not checked, so its exception is not stale.
        assert_eq!(r.stale_exceptions, ["[no-exec] gone.service in a"]);
    }

    #[test]
    fn reason_required() {
        assert!(parse("[[except]]\nrule='x'\nunit='y'\nreason=' '\n").is_err());
        assert!(parse("[[except]]\nrule='x'\nunit='y'\n").is_err());
    }
}
