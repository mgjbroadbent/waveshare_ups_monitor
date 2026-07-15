# waveshare-ups

A systemd service for the **Waveshare UPS Module 3S**. Reads the onboard INA219 over I2C, publishes
readings to MQTT with Home Assistant autodiscovery, and runs your scripts when the battery crosses a
threshold or mains power comes and goes.

- **Reports** bus voltage, current, power and battery percentage
- **Home Assistant** entities appear automatically via MQTT discovery
- **Hooks** let you stop services on low battery and start them again on recovery
- **Availability** is driven by an MQTT last will, so entities go unavailable if the daemon dies

## How battery percentage is calculated

The brief asked for two formulas: [Waveshare's `INA219.py:287`](https://www.waveshare.com/wiki/UPS_Module_3S)
(from the vendor's UPS Module 3S demo code) while charging, and
[ugursayar/ups_hat `ups_shutdown.py:128`](https://github.com/ugursayar/ups_hat/blob/master/ups_shutdown.py)
while discharging. Those turn out to be **the same formula at different cell counts**:

| Source | Empty → Full | Per cell |
|---|---|---|
| `INA219.py:287` — `(v-9)/3.6*100` | 9.0V → 12.6V | 3.0V → 4.2V (**3S**) |
| `ups_shutdown.py:128` — `(v-6.0)/2.4*100` | 6.0V → 8.4V | 3.0V → 4.2V (**2S**) |

Both interpolate 3.0 V/cell (empty) to 4.2 V/cell (full); that script even says so at its line 50
(`≈ 3.20 V / cell on 2S pack`). Using them literally on one pack would make the percentage jump at
every charge/discharge transition — on a 3S pack at 11.0V, 55.6% charging versus 100% discharging.
Normalised to the same pack they are identical, so the split would be decorative.

Charging and discharging *do* genuinely differ, but because of physics rather than arithmetic: a pack
reads high under charge and sags under load. So this service corrects for that cause directly, using
the measured current and the pack's internal resistance to recover open-circuit voltage:

```
V_oc = V_bus − (I × R_internal)      # charging I > 0 → subtract; discharging I < 0 → add
SoC  = clamp((V_oc − 9.0) / 3.6 × 100, 0, 100)
```

Charge current is positive and discharge negative, so **one expression handles both directions**, and
the second line is exactly the vendor formula applied to a corrected voltage. `internal_resistance_ohms`
is the one value worth tuning — see [Tuning](#tuning).

For a 2S pack, set `cells = 2`; the window becomes 6.0–8.4V and nothing else changes.

## Detecting mains power

`external_power` means **not discharging**, rather than "charging".

A full pack on mains tapers its charge current to nearly zero, so the obvious test — `current > 50mA`,
as the ugursayar script uses — reports *mains lost* on a perfectly healthy UPS and would fire your
power hooks spuriously. Because the Pi always draws current when running on battery, the reliable
test is the negative one:

- `charging` = `current > +charging_threshold_ma`
- `discharging` = `current < −discharging_threshold_ma`
- `external_power` = `!discharging` ← idle or full-on-mains correctly reads as powered

## Build

### On the Pi

Nothing special needed:

```sh
cargo build --release
```

### Cross-compiling from macOS

`cargo build --target aarch64-unknown-linux-gnu` **does not work on macOS**, even with the target
installed. It compiles fine but fails at the link step:

```
ld: unknown options: --as-needed -Bstatic -Bdynamic --eh-frame-hdr --gc-sections
```

`rustup target add` only supplies the Rust standard library; linking a Linux ELF still needs a Linux
linker, and Apple's `ld` does not understand GNU linker flags. Use
[`cargo-zigbuild`](https://github.com/rust-cross/cargo-zigbuild), which brings its own linker and
sysroot with no Docker required:

```sh
brew install zig
cargo install cargo-zigbuild
rustup target add aarch64-unknown-linux-gnu

# The .2.28 suffix pins the glibc floor. Without it the binary needs glibc 2.30+
# (Bullseye or newer); with it, Buster works too. It costs nothing, so prefer it.
cargo zigbuild --release --target aarch64-unknown-linux-gnu.2.28
```

The result lands in `target/aarch64-unknown-linux-gnu/release/waveshare-ups`. Sanity-check it before
copying:

```sh
file target/aarch64-unknown-linux-gnu/release/waveshare-ups
# ELF 64-bit LSB pie executable, ARM aarch64, ... dynamically linked, for GNU/Linux
```

(TLS is off by default, which keeps the `ring` C dependency out of the tree entirely — that is why no
C cross-toolchain is needed. Enabling `use-rustls` would reintroduce it.)

## Install

Copy the binary and packaging to the Pi, then:

```sh
sudo install -m 755 waveshare-ups /usr/local/bin/
sudo mkdir -p /etc/waveshare-ups/hooks
sudo install -m 644 packaging/config.toml.example /etc/waveshare-ups/config.toml
sudo install -m 755 packaging/hooks/*.sh /etc/waveshare-ups/hooks/
sudo install -m 644 packaging/waveshare-ups.service /etc/systemd/system/

sudo nano /etc/waveshare-ups/config.toml     # set mqtt.host
sudo systemctl enable --now waveshare-ups
journalctl -u waveshare-ups -f
```

I2C must be enabled (`sudo raspi-config` → Interface Options → I2C). Confirm the device is present:

```sh
i2cdetect -y 1     # expect 0x41 for the UPS Module 3S
```

If you see `0x42` instead, you likely have a 2S HAT: set `cells = 2` and `address = 0x42`.

## Configuration

See [`packaging/config.toml.example`](packaging/config.toml.example) — every option is documented
there. Only `mqtt.host` is required; everything else defaults to sensible values for a 3S module.

## Hooks

Set any of these to a script path in `[hooks]`; unset means no action.

| Event | Fires when |
|---|---|
| `on_battery_low` | drops below `low_threshold_pct` (20%) |
| `on_battery_ok` | recovers above `low_threshold_pct + hysteresis_pct` (25%) |
| `on_battery_critical` | drops below `critical_threshold_pct` (5%) |
| `on_battery_critical_clear` | recovers above `critical_threshold_pct + hysteresis_pct` (10%) |
| `on_power_lost` | mains goes away (starts discharging) |
| `on_power_restored` | mains returns |

Scripts receive:

| Variable | Example |
|---|---|
| `UPS_EVENT` | `battery_low` |
| `UPS_BATTERY_PCT` | `19.61` |
| `UPS_BUS_VOLTAGE` | `9.288` |
| `UPS_CURRENT_A` | `-0.650` (negative = discharging) |
| `UPS_POWER_W` | `6.037` |
| `UPS_CHARGING` | `1` / `0` |
| `UPS_EXTERNAL_POWER` | `1` / `0` |

Behaviour worth knowing:

- **The daemon never powers the machine off itself.** That policy belongs in
  `on_battery_critical` — see [the example](packaging/hooks/battery-critical.sh).
- **Hooks run serially**, so two never race over the same services.
- **A failing hook is logged, never fatal**, and never blocks the others — a broken
  `on_battery_low` still lets `on_battery_critical` run.
- **Hooks keep working when the broker is down.** Reporting and hook logic are independent.
- **Ordering**: power events lead; descending fires `low` then `critical`, ascending fires
  `critical_clear` then `ok`. So on the way down services stop before the drastic action, and on the
  way up the drastic action is undone first.
- **A tick can fire both `power_restored` and `battery_critical`** — mains can return while the pack
  is still nearly flat. Guard on `UPS_EXTERNAL_POWER` before shutting down, as the example does.
- **Hysteresis and debounce** stop a battery hovering at 20% from thrashing services: recovery must
  clear `threshold + hysteresis`, and any change must hold for `confirm_cycles` readings.

## Home Assistant

Six entities appear under one device, `Waveshare UPS (<device_id>)`:


| Entity | Type | Unit |
|---|---|---|
| Bus voltage | sensor (`voltage`) | V |
| Current | sensor (`current`) | A |
| Power | sensor (`power`) | W |
| Battery | sensor (`battery`) | % |
| External power | binary_sensor (`plug`) | — |
| Charging | binary_sensor (`battery_charging`) | — |

Discovery configs are retained, so HA repopulates them after a restart, and they are republished on
every reconnect. All six share one availability topic driven by the MQTT last will: if the daemon
crashes or the Pi loses power, the broker publishes `offline` and every entity goes unavailable
together. On an orderly `systemctl stop` the daemon publishes `offline` itself first — a clean MQTT
disconnect suppresses the will, so without that HA would show stale values as if it were still live.

### Naming

Anything sharing a global keyspace with other MQTT publishers is prefixed `waveshare-ups-`, because a
bare hostname is a poor key there — another integration on the same host publishing
`homeassistant/sensor/<hostname>/battery/config` would hit the identical topic and silently clobber
ours.

`device_id` is also **slugified** to `[a-zA-Z0-9_-]`, which Home Assistant requires for the `node_id`
and `object_id` levels of a discovery topic:

```
<discovery_prefix>/<component>/[<node_id>/]<object_id>/config
```

HA rejects a non-conforming config outright, and the entity simply never appears — with no obvious
error. A dotted hostname is the usual way to hit this, so `raspberrypi.local` becomes
`waveshare-ups-raspberrypi-local`. Slugifying applies to a configured `mqtt.device_id` too; one that
contains nothing usable is rejected at startup rather than silently renamed.

Note the dots are replaced rather than truncated at. `raspberrypi.local` → `raspberrypi` looks
tidier, but `pi.a.example` and `pi.b.example` would both collapse to `pi` and collide — exactly what
this scheme exists to prevent.

For a host called `raspberrypi.local`:

| | Value | Prefixed? |
|---|---|---|
| Discovery topic | `homeassistant/sensor/waveshare-ups-raspberrypi-local/battery/config` | yes — shared keyspace |
| Entity `unique_id` | `waveshare-ups-raspberrypi-local_battery` | yes — shared keyspace |
| MQTT client id | `waveshare-ups-raspberrypi-local` | yes — shared keyspace |
| Device identifier | `waveshare_ups_raspberrypi-local` | already namespaced |
| State topic | `waveshare-ups/raspberrypi-local/state` | already under `base_topic` |
| Device name | `Waveshare UPS (raspberrypi-local)` | display only |

The prefix applies whether `device_id` is derived from the hostname or set explicitly — it namespaces
this integration, not the hostname specifically. Our own topics keep the bare `device_id` rather than
stuttering into `waveshare-ups/waveshare-ups-raspberrypi-local/state`.

## Tuning

`internal_resistance_ohms` (default `0.20`) is the only value that usually needs attention. It should
equal your pack's real internal resistance — roughly 3× 18650 cells in series plus wiring.

To tune: watch `battery` while a large load starts or stops.

- Percentage **drops** when load is applied → resistance is too **low**, raise it
- Percentage **rises** when load is applied → too **high**, lower it
- Percentage barely moves → about right

`compensated_voltage` is published alongside the raw `bus_voltage` so you can see the correction
working.

## Development

The daemon runs on Linux, but the code builds and tests on macOS: the I2C HAL is target-gated behind
a `UpsSensor` trait, and `--simulate` swaps in a battery ramp.

```sh
cargo test                                     # 49 tests, no hardware needed
cargo check --target aarch64-unknown-linux-gnu # type-check the deploy target (does NOT link)
```

Note `cargo check` type-checks without linking, so it will happily pass on macOS while
`cargo build --target` fails at link time. To validate the real artefact, use `cargo zigbuild` as
above. The dead-code warnings you see on macOS are the cfg-gated Linux sensor; they disappear on the
Linux target.

To exercise MQTT, discovery and the whole hook chain with no UPS attached:

```sh
brew install mosquitto && brew services start mosquitto
cargo run -- --simulate --config packaging/config.toml.example
mosquitto_sub -t 'homeassistant/#' -t 'waveshare-ups/#' -v
```

### Notes on the hardware and the vendor driver

Some things found while porting `INA219.py` that are easy to trip over:

- **The vendor's comments contradict its code.** `INA219.py:204` claims `Cal = 13434` while `:206`
  writes `26868`. The code is right: 26868 satisfies `cal = 0.04096 / (lsb × r_shunt)` for
  `lsb = 0.1524 mA`, `r_shunt = 0.01 Ω`.
- **`set_calibration_16V_5A` cannot actually measure 5A.** The current register is signed 16-bit and
  saturates at ~4.995A; at 5.0A it wraps to −4.99A. Real hardware is safe — the chip raises its
  math-overflow flag and the driver errors out rather than reporting a sign-flipped value, which
  would otherwise look like discharging and fire the power hooks. (`INA219.py:215`'s claim of a
  3.2767A ceiling is stale copy-paste from `set_calibration_32V_2A`, and wrong too.)
- **The vendor's sign conversion is off by one LSB.** `if value > 32767: value -= 65535` should
  subtract 65536; this port uses proper two's complement.
- **`ina219`'s `IntCalibration` can't express this device.** It takes integer µA, but Waveshare's LSB
  is 152.**4** µA. We supply a small `Calibration` impl instead — the crate's intended extension
  point — carrying the exact vendor constants.
- **The `no_transaction` feature is mandatory on a Pi**, or I2C raises "operation not supported".
- **`ina219::calibration::simulate` only works for positive currents.** It passes the shunt register
  through `u32::from`, so a two's-complement negative reads as a huge positive and spuriously
  overflows. Discharge behaviour is tested against `current_from_register` directly.

## Running as a non-root user

The unit runs as root because it needs `/dev/i2c-1` and because hook scripts generally need
privilege to stop and start services. To run unprivileged instead, add a user to the `i2c` group and
set `User=` in the unit. Note the unit deliberately does **not** set `NoNewPrivileges=yes`, as that
would break `sudo` inside hook scripts.
