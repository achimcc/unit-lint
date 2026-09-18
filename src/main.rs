use std::path::{Path, PathBuf};
use std::process::ExitCode;

use unit_lint::config::{self, Config};
use unit_lint::live::{self, Host, LIVE_RULES, RealHost};
use unit_lint::rules::{self, RULES};
use unit_lint::unit;

const HELP: &str = "\
unit-lint — systemd units that are valid and still silently broken

USAGE:
    unit-lint check [OPTIONS] PATH...
    unit-lint live  [OPTIONS] [--machine NAME]... [--all-machines]
    unit-lint rules

check reads unit files before they are deployed. PATH is a NixOS toplevel
(its containers under etc/nixos-containers are checked too) or a directory
of unit files; write NAME=PATH to name it in the report.

live asks the running system — as root, on the host. --all-machines also
checks every running container (systemd-nspawn, via machinectl).

OPTIONS:
    -c, --config FILE   exceptions, see README (every one needs a reason)
    -r, --rule ID       run only this rule; repeatable
        --json          machine-readable report
    -v, --verbose       also list the findings covered by exceptions
    -h, --help
    -V, --version

EXIT STATUS:
    0  clean    1  findings or stale exceptions    2  usage or read error
";

#[derive(Default)]
struct Args {
    command: String,
    paths: Vec<String>,
    config: Option<PathBuf>,
    only: Vec<String>,
    machines: Vec<String>,
    all_machines: bool,
    json: bool,
    verbose: bool,
}

fn parse_args() -> Result<Option<Args>, lexopt::Error> {
    use lexopt::prelude::*;
    let mut a = Args::default();
    let mut p = lexopt::Parser::from_env();
    while let Some(arg) = p.next()? {
        match arg {
            Short('c') | Long("config") => a.config = Some(p.value()?.into()),
            Short('r') | Long("rule") => a.only.push(p.value()?.string()?),
            Long("machine") => a.machines.push(p.value()?.string()?),
            Long("all-machines") => a.all_machines = true,
            Long("json") => a.json = true,
            Short('v') | Long("verbose") => a.verbose = true,
            Short('h') | Long("help") => {
                print!("{HELP}");
                return Ok(None);
            }
            Short('V') | Long("version") => {
                println!("unit-lint {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            Value(v) if a.command.is_empty() => a.command = v.string()?,
            Value(v) => a.paths.push(v.string()?),
            _ => return Err(arg.unexpected()),
        }
    }
    Ok(Some(a))
}

fn load_config(path: Option<&Path>) -> Result<Config, String> {
    match path {
        None => Ok(Config::default()),
        Some(p) => {
            let text = std::fs::read_to_string(p).map_err(|e| format!("{}: {e}", p.display()))?;
            config::parse(&text).map_err(|e| format!("{}: {e}", p.display()))
        }
    }
}

fn print_rules() {
    println!("check (unit files):\n");
    for r in RULES {
        println!("{}\n  {}\n  {}\n", r.id, r.summary, r.explain);
    }
    println!("live (running system):\n");
    for (id, summary, explain) in LIVE_RULES {
        println!("{id}\n  {summary}\n  {explain}\n");
    }
}

fn run(a: &Args) -> Result<bool, String> {
    let config = load_config(a.config.as_deref())?;
    let known: Vec<&str> = RULES
        .iter()
        .map(|r| r.id)
        .chain(LIVE_RULES.iter().map(|r| r.0))
        .collect();
    if let Some(bad) = a.only.iter().find(|o| !known.contains(&o.as_str())) {
        return Err(format!("unknown rule {bad} — see `unit-lint rules`"));
    }
    if let Some(e) = config
        .exceptions
        .iter()
        .find(|e| !known.contains(&e.rule.as_str()))
    {
        return Err(format!("exception names unknown rule {}", e.rule));
    }
    let selected = |id: &&str| a.only.is_empty() || a.only.iter().any(|o| o == id);

    let (findings, rules_run, systems, units): (Vec<_>, Vec<&str>, Vec<String>, usize) =
        match a.command.as_str() {
            "check" => {
                if a.paths.is_empty() {
                    return Err("check needs at least one PATH".into());
                }
                let mut systems = Vec::new();
                for p in &a.paths {
                    let (label, path) = match p.split_once('=') {
                        Some((l, path)) => (l.to_owned(), PathBuf::from(path)),
                        None => (p.clone(), PathBuf::from(p)),
                    };
                    let loaded = if path.join("etc/systemd/system").is_dir() {
                        unit::load_toplevel(&label, &path)
                    } else {
                        unit::load(&label, &path, false).map(|s| vec![s])
                    };
                    systems.extend(loaded.map_err(|e| format!("{}: {e}", path.display()))?);
                }
                let findings = systems
                    .iter()
                    .flat_map(|s| rules::check(s, &a.only))
                    .collect();
                let units = systems.iter().map(|s| s.units.len()).sum();
                let rules_run = RULES.iter().map(|r| r.id).filter(selected).collect();
                (
                    findings,
                    rules_run,
                    systems.into_iter().map(|s| s.name).collect(),
                    units,
                )
            }
            "live" => {
                let host = RealHost::new();
                let mut targets: Vec<Option<String>> = vec![None];
                targets.extend(a.machines.iter().cloned().map(Some));
                if a.all_machines {
                    targets.extend(host.machines()?.into_iter().map(Some));
                }
                let mut findings = Vec::new();
                let mut units = 0;
                let mut names = Vec::new();
                for t in &targets {
                    let (n, f) = live::check_system(&host, t.as_deref())?;
                    units += n;
                    names.push(t.clone().unwrap_or_else(|| "host".into()));
                    findings.extend(f.into_iter().filter(|f| selected(&f.rule)));
                }
                let rules_run = LIVE_RULES.iter().map(|r| r.0).filter(selected).collect();
                (findings, rules_run, names, units)
            }
            "" => return Err("missing command: check, live or rules".into()),
            other => return Err(format!("unknown command {other}")),
        };

    let report = config::apply(
        &config,
        findings,
        &rules_run,
        &systems,
        systems.len(),
        units,
    );
    if a.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        print!("{}", report.text(a.verbose));
    }
    Ok(!report.failed())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("unit-lint: {e}\nTry `unit-lint --help`.");
            return ExitCode::from(2);
        }
    };
    if args.command == "rules" {
        print_rules();
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("unit-lint: {e}");
            ExitCode::from(2)
        }
    }
}
