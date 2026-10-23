use std::path::{Path, PathBuf};
use std::process::ExitCode;

use unit_lint::config::{self, Config};
use unit_lint::live::{self, Host, LIVE_RULES, RealHost};
use unit_lint::rules::{self, RULES};
use unit_lint::timers::{self, RealTimerHost, TimerHost};
use unit_lint::tools;
use unit_lint::unit;

const HELP: &str = "\
unit-lint — systemd units that are valid and still silently broken

USAGE:
    unit-lint check [OPTIONS] PATH...
    unit-lint live  [OPTIONS] [--machine NAME]... [--all-machines]
    unit-lint timers [--textfile PATH] [--host-label NAME] [--machine NAME]... [--all-machines]
    unit-lint tools  [OPTIONS] [--rust-only] TOPLEVEL --sources DIR...
    unit-lint tools  --self-test
    unit-lint rules

check reads unit files before they are deployed. PATH is a NixOS toplevel
(its containers under etc/nixos-containers are checked too) or a directory
of unit files; write NAME=PATH to name it in the report.

live asks the running system — as root, on the host. --all-machines also
checks every running container (systemd-nspawn, via machinectl).

timers reads every timer of the host (and, with --machine/--all-machines,
of its containers) and prints Prometheus metrics: when it last fired, when
it fires next, its result, and the state of the service it starts. Meant to
run from a timer of its own, as root, into node_exporter's textfile
directory. A machine that cannot be read is reported as
unit_lint_timers_scrape_ok 0, never left out.

tools asks whether every program that our own sources start in a guest
over the PATH is in that guest's system profile (`<SYSTEM_PATH>/sw/bin`,
i.e. environment.systemPackages). A unit's ExecStart carries the store path
and runs regardless; a hand or the guest check gets 203/EXEC. Call sites
are read from the guest check's modules under
pkgs/gast-pruefung/src/gaeste/ (`a.gast(GAST, <prog>, …)`, `a.gast_mit`,
`a.als`; the guest is the module, or a literal in the call) and from the
living Markdown (README.md, CLAUDE.md, docs/runbooks, docs/werkzeuge,
.claude/skills, .claude/agents: `vantage run <guest> … -- prog`). Line
comments, store paths and shell builtins are skipped. A call whose program
(or an argument before it) is not a literal cannot be read; it is reported
as `tool-unparsed` (unit = file:line) rather than skipped. Zero call sites
is an error, not a clean run. Exceptions use rule `tool-missing`, `unit` =
the program (or `tool-unparsed`, `unit` = file:line), and may name `file`
to cover one source file only.

TOOLS OPTIONS:
        --sources DIR       the source tree to scan; repeatable
        --rust-only         skip the Markdown
        --self-test         run the scan over a built-in fixture that must
                            go red, and nothing else

TIMERS OPTIONS:
        --textfile PATH     write the metrics there (temporary file + rename)
                            instead of stdout
        --host-label NAME   value of the machine label for the host [host]

OPTIONS:
    -c, --config FILE   exceptions, see README (every one needs a reason)
    -r, --rule ID       run only this rule; repeatable
        --json          machine-readable report
    -v, --verbose       also list the findings covered by exceptions
    -h, --help
    -V, --version

EXIT STATUS:
    0  clean    1  findings or stale exceptions    2  usage or read error
    timers: 0  every machine read    1  a machine could not be read    2  usage or write error
    tools:  2 also when no call site was found, or the self-test did not go red
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
    textfile: Option<PathBuf>,
    host_label: String,
    sources: Vec<PathBuf>,
    rust_only: bool,
    self_test: bool,
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
            Long("textfile") => a.textfile = Some(p.value()?.into()),
            Long("host-label") => a.host_label = p.value()?.string()?,
            Long("sources") => a.sources.push(p.value()?.into()),
            Long("rust-only") => a.rust_only = true,
            Long("self-test") => a.self_test = true,
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
    println!("tools (sources against the built guests):\n");
    println!(
        "{}\n  a program our sources start in a guest over the PATH is not in that guest's profile\n  ExecStart carries the store path, a hand or the guest check gets 203/EXEC — add it to environment.systemPackages\n",
        tools::TOOL_RULE
    );
    println!(
        "{}\n  a call of a.gast/a.gast_mit/a.als the scanner could not read (program or an argument before it is not a literal)\n  a silent gap is what this check exists to prevent: name the program as a literal, or except the call with a reason\n",
        tools::UNPARSED_RULE
    );
}

fn run(a: &Args) -> Result<bool, String> {
    let config = load_config(a.config.as_deref())?;
    let known: Vec<&str> = RULES
        .iter()
        .map(|r| r.id)
        .chain(LIVE_RULES.iter().map(|r| r.0))
        .chain([tools::TOOL_RULE, tools::UNPARSED_RULE])
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
            "tools" => {
                let top = match a.paths.as_slice() {
                    [one] => PathBuf::from(one),
                    [] => return Err("tools needs a TOPLEVEL (or --self-test)".into()),
                    more => return Err(format!("tools takes one TOPLEVEL, got {}", more.len())),
                };
                if a.sources.is_empty() {
                    return Err("tools needs --sources DIR".into());
                }
                let mut sites = Vec::new();
                let mut unparsed = Vec::new();
                for root in &a.sources {
                    let scan = tools::scan_all(root, a.rust_only)
                        .map_err(|e| format!("{}: {e}", root.display()))?;
                    sites.extend(scan.sites);
                    unparsed.extend(scan.unparsed);
                }
                if sites.is_empty() {
                    return Err(format!(
                        "no call sites found under --sources {} — wrong directory?",
                        a.sources
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
                let (mut findings, guests) =
                    tools::check(&top, &sites).map_err(|e| format!("{}: {e}", top.display()))?;
                eprintln!(
                    "unit-lint tools: {} call sites, {} unparsed, {} guests",
                    sites.len(),
                    unparsed.len(),
                    guests.len()
                );
                findings.extend(tools::unparsed_findings(&unparsed));
                let findings = findings.into_iter().filter(|f| selected(&f.rule)).collect();
                let rules_run = [tools::TOOL_RULE, tools::UNPARSED_RULE]
                    .iter()
                    .copied()
                    .filter(selected)
                    .collect();
                (findings, rules_run, guests, sites.len())
            }
            "" => return Err("missing command: check, live, timers, tools or rules".into()),
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

/// `timers`: metrics for every timer of the host and the chosen machines.
/// Writes what it has even when a machine failed — that machine shows up
/// as `scrape_ok 0` — and returns false then.
fn run_timers(a: &Args) -> Result<bool, String> {
    if !a.paths.is_empty() {
        return Err(format!("timers takes no PATH (got {})", a.paths.join(" ")));
    }
    let host = RealTimerHost;
    let mut machines = a.machines.clone();
    if a.all_machines {
        machines.extend(host.machines()?);
    }
    let label = if a.host_label.is_empty() {
        "host"
    } else {
        a.host_label.as_str()
    };
    let read = timers::collect_all(&host, label, &machines);
    let text = timers::render(&read);
    match &a.textfile {
        Some(p) => timers::write_textfile(p, &text)?,
        None => print!("{text}"),
    }
    let mut ok = true;
    for m in &read {
        if let Err(e) = &m.timers {
            eprintln!("unit-lint: {}: {e}", m.machine);
            ok = false;
        }
    }
    Ok(ok)
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
    if args.command == "tools" && args.self_test {
        return match tools::self_test() {
            Ok(line) => {
                println!("{line}");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("unit-lint: {e}");
                ExitCode::from(2)
            }
        };
    }
    let result = if args.command == "timers" {
        run_timers(&args)
    } else {
        run(&args)
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("unit-lint: {e}");
            ExitCode::from(2)
        }
    }
}
