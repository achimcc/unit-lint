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

**live** — asks the running system (as root; `--all-machines` includes
every running systemd-nspawn container):

| Rule | Finds |
|---|---|
| `orphan-process` | `Loaded: not-found` and `Active: active (running)` at once — a deploy removed the unit, not the process. |
| `not-loadable` | a unit systemd refused to load (`bad-setting`, `error`). |
| `stale-sandbox` | a process holding capabilities its unit no longer grants: the unit changed, the process was never restarted. A process can only drop capabilities from its bounding set, never gain them, so this is exact. |

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
unit-lint rules

  -c, --config FILE   exceptions
  -r, --rule ID       run only this rule; repeatable
      --json          machine-readable report
  -v, --verbose       also list excepted findings
```

Exit status: `0` clean, `1` findings or stale exceptions, `2` usage or read
error.

## Limits

- `oneshot-waits-in-boot` reads the `ExecStart=` script and looks for an
  `until`/`while` loop with a `sleep`. It sees shell scripts, not a wait
  hidden in a compiled program.
- `check` reads only the unit directory it is given. Units that exist only
  at runtime (generators, transient units) are unknown to it.
- `live` needs root, and for containers `machinectl`.

## Install

```sh
nix run github:achimcc/unit-lint -- rules
cargo install --git https://github.com/achimcc/unit-lint
```

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
