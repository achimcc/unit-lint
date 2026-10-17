# unit-lint

Finds systemd units that are valid and still silently broken — in a built
NixOS system before it is deployed, and on the running host afterwards.

Every rule here stands for a failure that stayed invisible: `systemctl
--failed` was empty, the deploy was green, the service said it was fine.
The unit had loaded, or had never been loadable and therefore was not
`failed` either; the timer fired and nothing ran; the process ran with a
sandbox from an earlier deploy. Each of them was found in a real homelab by
a person who clicked on something, hours or days later.

```console
$ nix build .#nixosConfigurations.server.config.system.build.toplevel
$ unit-lint check server=result
unit-lint: 34 system(s), 6886 unit(s)
  server  local-fs.target  [on-failure-missing] OnFailure=emergency.target, which does not exist
  server  systemd-fsck-root.service  [on-failure-missing] OnFailure=emergency.target, which does not exist
  auth-01  authentik-ldap-token.service  [oneshot-restart-forever] Restart=on-failure, and the start limit cannot trip: 5 restarts 30s apart never fit into 10s
  jelly-01  jellyfin-newsletter.service  [oneshot-restart-forever] Restart=on-failure, and the start limit cannot trip: 5 restarts 60s apart never fit into 10s
  jelly-01  jellarr-api-key-bootstrap.service  [oneshot-waits-in-boot] multi-user.target waits for it, it has no start timeout, and jellarr-api-key-bootstrap-start polls in a loop
5 finding(s), 0 stale exception(s), 0 excepted — `unit-lint rules` explains each rule
```

A NixOS toplevel brings its declarative containers along
(`etc/nixos-containers/*.conf`); they are checked as systems of their own.

## Rules

`unit-lint rules` prints each with its explanation.

**check** — reads unit files:

| Rule | Finds |
|---|---|
| `no-exec` | a service without `ExecStart=`, `ExecStop=` or `SuccessAction=`. systemd refuses to load it (`bad-setting`), which is not `failed`. In NixOS usually `lib.mkIf` one level too deep. |
| `dropin-without-unit` | settings for a unit that does not exist. Units that systemd's generators write at boot (`systemd-makefs@`, `sshd-vsock`, …) are known and skipped. |
| `timer-never-reruns` | a repeating timer on a service with `RemainAfterExit=yes`: it stays active after the first run, and starting an active unit does nothing. |
| `timer-unit-missing` | a timer whose unit does not exist. |
| `on-failure-missing` | `OnFailure=`/`OnSuccess=` naming a unit that does not exist — an alarm that cannot fire. |
| `oneshot-restart-forever` | `Type=oneshot` with `Restart=`, where the restart delay is too long for the start limit to ever trip. It never becomes `failed`, and everything waiting for it waits forever. |
| `oneshot-waits-in-boot` | in a container: a oneshot in `multi-user.target` without a start timeout whose script polls in a loop. The container never finishes booting. |
| `podman-mount-namespace` | a podman unit with a sandbox option that gives it a mount namespace. ExecStop cannot enter the container's network namespace, and the stale DNAT rule shadows the port. |
| `implicit-errexit` | a NixOS unit script (`script`, `preStart`, `postStop` …) that opens with its own `set -uo pipefail`. NixOS has already put `set -e` in front of it, so the first failing assignment ends the script silently and the error handling below is dead text. Say `set -euo pipefail` or `set +e`. |

**live** — asks the running system (as root; `--all-machines` includes
every running systemd-nspawn container):

| Rule | Finds |
|---|---|
| `orphan-process` | `Loaded: not-found` and `Active: active (running)` at once — a deploy removed the unit, not the process. |
| `not-loadable` | a unit systemd refused to load (`bad-setting`, `error`). |
| `stale-sandbox` | a process holding capabilities its unit no longer grants: the unit changed, the process was never restarted. A process can only drop capabilities from its bounding set, never gain them, so this is exact. |

**timers** — the running system's timers as Prometheus metrics (as root;
`--machine`/`--all-machines` as for `live`):

```console
$ unit-lint timers --all-machines --host-label wirt --textfile /var/lib/node-exporter-textfile/unit-lint-timers.prom
```

A timer that stopped firing, a timer service that ends with exit 0 before
its second job, a service left in `inactive (dead)` after a clean exit, a
oneshot with `RemainAfterExit=yes` that its timer can never start again —
none of them is `failed`, and none of them turns anything red. Written to
node_exporter's textfile directory every minute, they become series an
alarm can watch (`time() - unit_lint_timer_last_trigger_seconds > 2 *
interval`). node_exporter's own `node_systemd_timer_*` sees only the host;
this sees every machine, and a machine it could not read is a series, not
silence.

Every metric is a gauge. `machine` is the container's name, or
`--host-label` (default `host`) for the host; `timer` and `service` are the
unit names.

| Metric | Labels | Value |
|---|---|---|
| `unit_lint_timers_scrape_ok` | `machine` | 1 when the machine's timers could be read, 0 when `systemctl` failed there |
| `unit_lint_timers_seen` | `machine` | number of timers systemd lists |
| `unit_lint_timer_last_trigger_seconds` | `machine`, `timer`, `service` | Unix time the timer last fired; 0 never |
| `unit_lint_timer_next_elapse_seconds` | 〃 | Unix time it fires next; 0 when it will not fire again (`OnBootSec=` after boot, `OnUnitActiveSec=` on a service that stays active) |
| `unit_lint_timer_active` | 〃 | 1 when the timer unit is active |
| `unit_lint_timer_result` | 〃, `result` | 1 on the timer's current `Result=` |
| `unit_lint_timer_service_loaded` | 〃 | 1 when the activated service exists |
| `unit_lint_timer_service_active` | 〃 | 1 while the service runs |
| `unit_lint_timer_service_result` | 〃, `result` | 1 on the service's current `Result=` (`success`, `exit-code`, `timeout`, …) |
| `unit_lint_timer_service_last_exit_seconds` | 〃 | Unix time its main process last exited; 0 never |
| `unit_lint_timer_service_exit_status` | 〃 | exit status at that exit |
| `unit_lint_timer_service_restarts` | 〃 | `NRestarts=` |
| `unit_lint_timer_service_remain_after_exit` | 〃 | 1 when the service has `RemainAfterExit=yes` — the live face of `timer-never-reruns` |

```
# HELP unit_lint_timers_scrape_ok 1 when the machine's timers could be read, 0 when systemctl failed there.
# TYPE unit_lint_timers_scrape_ok gauge
unit_lint_timers_scrape_ok{machine="wirt"} 1
unit_lint_timers_scrape_ok{machine="down-01"} 0
unit_lint_timers_scrape_ok{machine="infra-01"} 1
# HELP unit_lint_timer_last_trigger_seconds Unix time the timer last fired; 0 when it never did.
# TYPE unit_lint_timer_last_trigger_seconds gauge
unit_lint_timer_last_trigger_seconds{machine="infra-01",timer="logrotate.timer",service="logrotate.service"} 1792216813
# HELP unit_lint_timer_service_remain_after_exit 1 when the service has RemainAfterExit=yes: it stays active after the first run, and the timer's later starts do nothing.
# TYPE unit_lint_timer_service_remain_after_exit gauge
unit_lint_timer_service_remain_after_exit{machine="wirt",timer="kernel-modulsperre.timer",service="kernel-modulsperre.service"} 1
```

Sources: `systemctl [-M …] list-timers --all --output=json` for the list
and the `last`/`next` times (it folds monotonic timers in, which `show`'s
`NextElapseUSecRealtime` does not), and `systemctl show --timestamp=unix
-p …` for states, results and the service's exit — parsed by name, never
by position, because with several `-p` systemd prints in its own order.
`--textfile` writes through a temporary file and a rename.

## What it deliberately does not check

**Several `CapabilityBoundingSet=` lines.** They look like the last one
wins, and for transient units (`systemd-run -p`) it does. In unit *files*
the lines are ORed together (`man systemd.exec`). A rule for it would flag
working sandboxes; `stale-sandbox` compares what is actually in effect.

## Exceptions

A finding that is deliberate says so — with a reason:

```toml
[[except]]
rule = "on-failure-missing"
unit = "local-fs.target"
system = "server"            # optional: every system when missing
reason = "systemd.enableEmergencyMode = false — there is no emergency.target on purpose"
```

`unit` may end in `*` (`podman-*`). An exception without a reason is an
error, and one that matches nothing in a run that could have matched it is
reported as stale — so the list cannot outlive its reasons quietly.

## Usage

```
unit-lint check [OPTIONS] PATH...     NixOS toplevel or unit directory; NAME=PATH to label
unit-lint live  [OPTIONS] [--machine NAME]... [--all-machines]
unit-lint timers [--textfile PATH] [--host-label NAME] [--machine NAME]... [--all-machines]
unit-lint rules

  -c, --config FILE   exceptions
  -r, --rule ID       run only this rule; repeatable
      --json          machine-readable report
  -v, --verbose       also list excepted findings
      --textfile PATH   timers: write the metrics there instead of stdout
      --host-label NAME timers: machine label of the host [host]
```

Exit status: `0` clean, `1` findings or stale exceptions, `2` usage or read
error. For `timers`: `0` every machine read, `1` a machine could not be read
(the metrics are still written, with `scrape_ok 0` for it), `2` usage or
write error.

## Limits

- `oneshot-waits-in-boot` reads the `ExecStart=` script and looks for an
  `until`/`while` loop with a `sleep`. It sees shell scripts, not a wait
  hidden in a compiled program.
- `implicit-errexit` only looks at scripts NixOS generated — a shebang
  followed by `set -e` — and only when the text after that opens with `set`
  lines of its own. A script without any `set` line says nothing about what
  its author assumed and is left alone; so is a hand-written wrapper
  (`writeShellScript`), which gets no `set -e` from NixOS. Any later line
  starting with `set` that names errexit counts as the declaration, even
  inside a here-document.
- `check` reads only the unit directory it is given. Units that exist only
  at runtime (generators, transient units) are unknown to it.
- `live` and `timers` need root, and for containers `machinectl`.
- `timers` reads timestamps with `systemctl show --timestamp=unix`
  (systemd ≥ 250) and lists machines with `machinectl list --output=json`.

## Install

```sh
nix run github:achimcc/unit-lint -- rules
cargo install --git https://github.com/achimcc/unit-lint
```

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
