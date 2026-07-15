# waveshare_ups

Deploys the [Waveshare UPS Module 3S monitor](../../../README.md) as a systemd unit: installs the
executable, renders `/etc/waveshare-ups/config.toml`, installs the hook scripts, and enables the
service.

The role reproduces the manual install documented in the project README, so the daemon's own
defaults are the role's defaults. **Only `waveshare_ups_mqtt_host` has to be set.**

## Shipping the executable

The binary is an input to the role, because it is a build artefact rather than source. The role
never builds it — see [Build](../../../README.md#build), and note that a plain
`cargo build --target aarch64-unknown-linux-gnu` does not link on macOS.

`waveshare_ups_binary_src` follows Ansible's usual `copy` semantics, which covers both cases with
one variable:

**Inside the role** — drop the binary in the role's own `files/`, and the default just works:

```sh
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.28
cp target/aarch64-unknown-linux-gnu/release/waveshare-ups \
   ansible/roles/waveshare_ups/files/
```

The role is then self-contained and can be archived, vendored, or committed to a private repo as one
unit. (This path is `.gitignore`d here, since a binary is not source.)

**Outside the role** — point at an absolute path, and nothing needs to live in `files/`:

```yaml
waveshare_ups_binary_src: /home/mark/Code/waveshare-ups/target/aarch64-unknown-linux-gnu/release/waveshare-ups
```

A relative path resolves against the role's `files/` directory; an absolute path is used as-is. If
neither exists the role stops before it installs anything, with a message telling you how to build
it, rather than failing midway. Once installed, the role runs `waveshare-ups --help` on the target: a
wrong-architecture or wrong-glibc build fails there, with the loader's own error, instead of leaving
a unit that restarts forever.

`waveshare_ups_hooks_src` works the same way and defaults to the role's `files/hooks/`.

## Requirements

- A Debian-family target (Raspberry Pi OS) with systemd.
- I2C enabled: `sudo raspi-config` → Interface Options → I2C. The role asserts
  `/dev/i2c-<bus>` exists and stops with instructions if it does not; set
  `waveshare_ups_check_i2c: false` to skip.
- `become: true` on the play — the role installs to `/usr/local/bin` and `/etc`.

## Role variables

Only the ones specific to the role are listed here. Everything under
[`[i2c]`, `[battery]`, `[mqtt]`, `[monitor]` and `[hooks]`](../../../packaging/config.toml.example)
is exposed as `waveshare_ups_<key>` with the daemon's default — for example
`waveshare_ups_battery_internal_resistance_ohms` (`0.20`), which is
[the one value worth tuning](../../../README.md#tuning). See [`defaults/main.yml`](defaults/main.yml)
for the full list.

| Variable | Default | Notes |
|---|---|---|
| `waveshare_ups_mqtt_host` | `""` | **Required**; the only value with no default |
| `waveshare_ups_binary_src` | `waveshare-ups` | Relative → role `files/`; absolute → used as-is |
| `waveshare_ups_hooks_src` | `hooks` | Same rule; `""` to manage the hooks directory yourself |
| `waveshare_ups_binary_path` | `/usr/local/bin/waveshare-ups` | |
| `waveshare_ups_config_dir` | `/etc/waveshare-ups` | |
| `waveshare_ups_user` | `root` | Needs `/dev/i2c-N`, and hooks need privilege. A named user must be in the `i2c` group |
| `waveshare_ups_log_level` | `info` | `RUST_LOG` in the unit |
| `waveshare_ups_check_i2c` | `true` | Turn off for images that enable I2C on a later boot |
| `waveshare_ups_service_enabled` | `true` | |
| `waveshare_ups_service_state` | `started` | `stopped` also suppresses the restart handler |

The rendered config is mode `0600` because `waveshare_ups_mqtt_password` lands in it. Keep that
password in a vault rather than in a playbook:

```yaml
waveshare_ups_mqtt_username: ups
waveshare_ups_mqtt_password: "{{ vault_waveshare_ups_mqtt_password }}"
```

## Hooks

The role ships the example scripts from [`packaging/hooks/`](../../../packaging/hooks/) and wires
them up, matching `config.toml.example`. **Note that `battery-critical.sh` powers the machine off**
at 5% — that is the point of it, and it is guarded on `UPS_EXTERNAL_POWER`, but it is the one default
worth a look before you run this against a fleet. To deploy your own instead, point
`waveshare_ups_hooks_src` at a directory of your own scripts; to leave an event unhandled, set its
variable to `~`:

```yaml
waveshare_ups_hooks_src: "{{ playbook_dir }}/files/ups-hooks"
waveshare_ups_on_battery_critical: ~
```

Hook changes need no restart — the daemon executes them fresh on each event — so the role does not
notify one. Config, unit and binary changes all restart the service.

## Checking variables without installing

The validation mirrors the daemon's own `validate()` and lives in its own tasks file, so it can be
run against a host on its own:

```yaml
- ansible.builtin.include_role:
    name: waveshare_ups
    tasks_from: validate
```

## Idempotence

Reruns are no-ops. The service restarts only when the binary, the rendered config or the unit
actually changes; a restart is not free, since the daemon publishes `offline` to MQTT on the way
down and every Home Assistant entity blinks unavailable.
