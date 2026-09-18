use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct Finding {
    pub rule: &'static str,
    pub system: String,
    pub unit: String,
    pub message: String,
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub systems: usize,
    pub units: usize,
    pub findings: Vec<Finding>,
    /// Findings covered by an exception, with its reason.
    pub excepted: Vec<(Finding, String)>,
    /// Exceptions that matched nothing. An exception outlives its reason
    /// silently otherwise, and then it hides the next real finding.
    pub stale_exceptions: Vec<String>,
}

impl Report {
    pub fn failed(&self) -> bool {
        !self.findings.is_empty() || !self.stale_exceptions.is_empty()
    }

    pub fn text(&self, verbose: bool) -> String {
        let mut s = format!(
            "unit-lint: {} system(s), {} unit(s)\n",
            self.systems, self.units
        );
        for f in &self.findings {
            s += &format!("  {}  {}  [{}] {}\n", f.system, f.unit, f.rule, f.message);
        }
        for e in &self.stale_exceptions {
            s += &format!("  stale exception: {e}\n");
        }
        if verbose {
            for (f, reason) in &self.excepted {
                s += &format!(
                    "  (excepted) {}  {}  [{}] — {reason}\n",
                    f.system, f.unit, f.rule
                );
            }
        }
        s += &match (self.findings.len(), self.stale_exceptions.len()) {
            (0, 0) => format!("clean ({} excepted)\n", self.excepted.len()),
            (n, m) => format!(
                "{n} finding(s), {m} stale exception(s), {} excepted — `unit-lint rules` explains each rule\n",
                self.excepted.len()
            ),
        };
        s
    }
}
