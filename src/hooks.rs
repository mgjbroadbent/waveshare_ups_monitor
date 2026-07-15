//! Threshold latches and the external-script runner.
//!
//! Two mechanisms stop a battery sitting on a threshold from flapping services up and down:
//! *hysteresis* (recovery must clear `threshold + hysteresis`, not just `threshold`) and *debounce*
//! (`confirm_cycles` consecutive readings before any transition is accepted).
//!
//! The latched state is also the state we *report*, via `reported` -- see its comment. Anything that
//! tells the outside world what the UPS is doing should agree with what the hooks acted on.

use crate::battery::Reading;
use crate::config::HooksConfig;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    BatteryLow,
    BatteryOk,
    BatteryCritical,
    BatteryCriticalClear,
    PowerLost,
    PowerRestored,
}

impl Event {
    pub fn as_str(self) -> &'static str {
        match self {
            Event::BatteryLow => "battery_low",
            Event::BatteryOk => "battery_ok",
            Event::BatteryCritical => "battery_critical",
            Event::BatteryCriticalClear => "battery_critical_clear",
            Event::PowerLost => "power_lost",
            Event::PowerRestored => "power_restored",
        }
    }

    fn script(self, cfg: &HooksConfig) -> Option<&PathBuf> {
        match self {
            Event::BatteryLow => cfg.on_battery_low.as_ref(),
            Event::BatteryOk => cfg.on_battery_ok.as_ref(),
            Event::BatteryCritical => cfg.on_battery_critical.as_ref(),
            Event::BatteryCriticalClear => cfg.on_battery_critical_clear.as_ref(),
            Event::PowerLost => cfg.on_power_lost.as_ref(),
            Event::PowerRestored => cfg.on_power_restored.as_ref(),
        }
    }
}

/// A debounced boolean with hysteresis on the recovery edge.
#[derive(Debug)]
struct Latch {
    /// None until the first reading seeds it.
    tripped: Option<bool>,
    pending: u32,
}

impl Latch {
    fn new() -> Self {
        Self {
            tripped: None,
            pending: 0,
        }
    }

    /// `raw_tripped` is the instantaneous condition, already accounting for hysteresis. Returns
    /// `Some(new_state)` only once the condition has held for `confirm_cycles` readings.
    fn update(&mut self, raw_tripped: bool, confirm_cycles: u32) -> Option<bool> {
        let Some(current) = self.tripped else {
            // First reading: adopt reality immediately rather than debouncing against nothing.
            self.tripped = Some(raw_tripped);
            return Some(raw_tripped);
        };

        if raw_tripped == current {
            self.pending = 0;
            return None;
        }

        self.pending += 1;
        if self.pending >= confirm_cycles.max(1) {
            self.tripped = Some(raw_tripped);
            self.pending = 0;
            return Some(raw_tripped);
        }
        None
    }
}

pub struct HookMachine {
    low: Latch,
    critical: Latch,
    power: Latch,
    /// Debounced for reporting only -- no hook fires on a charge/discharge change. It is latched
    /// because MQTT publishes it as a binary_sensor, and it flaps for exactly the same reason
    /// `power` does.
    charging: Latch,
    low_threshold: f64,
    critical_threshold: f64,
    hysteresis: f64,
    confirm_cycles: u32,
    fire_on_startup: bool,
    started: bool,
}

impl HookMachine {
    pub fn new(hooks: &HooksConfig, confirm_cycles: u32) -> Self {
        Self {
            low: Latch::new(),
            critical: Latch::new(),
            power: Latch::new(),
            charging: Latch::new(),
            low_threshold: hooks.low_threshold_pct,
            critical_threshold: hooks.critical_threshold_pct,
            hysteresis: hooks.hysteresis_pct,
            confirm_cycles,
            fire_on_startup: hooks.fire_on_startup,
            started: false,
        }
    }

    /// Events are ordered so that on the way down services are stopped before the drastic action
    /// (low then critical), and on the way up the drastic action is undone first
    /// (critical_clear then ok). Power events always lead.
    pub fn update(&mut self, reading: &Reading) -> Vec<Event> {
        let is_startup = !self.started;
        self.started = true;

        let mut events = Vec::new();

        if let Some(lost) = self.power.update(!reading.external_power, self.confirm_cycles) {
            events.push(if lost {
                Event::PowerLost
            } else {
                Event::PowerRestored
            });
        }

        // Emits nothing; kept in step purely so `reported` has a debounced value to hand out.
        self.charging.update(reading.charging, self.confirm_cycles);

        let low_now = Self::trip(self.low.tripped, reading.battery_pct, self.low_threshold, self.hysteresis);
        let low_event = self
            .low
            .update(low_now, self.confirm_cycles)
            .map(|t| if t { Event::BatteryLow } else { Event::BatteryOk });

        let crit_now = Self::trip(
            self.critical.tripped,
            reading.battery_pct,
            self.critical_threshold,
            self.hysteresis,
        );
        let crit_event = self
            .critical
            .update(crit_now, self.confirm_cycles)
            .map(|t| {
                if t {
                    Event::BatteryCritical
                } else {
                    Event::BatteryCriticalClear
                }
            });

        // Descending: low before critical. Ascending: critical_clear before ok.
        match (low_event, crit_event) {
            (Some(l @ Event::BatteryLow), Some(c)) => events.extend([l, c]),
            (Some(l), Some(c)) => events.extend([c, l]),
            (Some(e), None) | (None, Some(e)) => events.push(e),
            (None, None) => {}
        }

        if is_startup && !self.fire_on_startup {
            // Latches are seeded above; suppress the events so restarting the daemon does not tear
            // down services that are already in the right state.
            if !events.is_empty() {
                info!(
                    suppressed = ?events,
                    "initial state latched without firing hooks (hooks.fire_on_startup = false)"
                );
            }
            return Vec::new();
        }

        events
    }

    /// `reading` with the instantaneous binary states replaced by their latched equivalents.
    ///
    /// `Reading::external_power` and `charging` are each derived from one sample's current, so a
    /// momentary load transient past the deadband flips them for a single tick. The hooks have
    /// always been shielded from that by the latches; publishing the raw `Reading` meant Home
    /// Assistant was not, and showed mains dropouts the hooks had correctly ignored. Reporting the
    /// latched view instead is what keeps the two telling the same story.
    ///
    /// The numeric fields are passed through untouched: they are genuine per-sample measurements,
    /// and `battery_pct` already has the EMA behind it.
    ///
    /// Call after `update`, which seeds the latches. Before that the raw values pass through, which
    /// is the same value `update` would have latched from this reading anyway.
    pub fn reported(&self, reading: &Reading) -> Reading {
        Reading {
            external_power: self
                .power
                .tripped
                .map_or(reading.external_power, |lost| !lost),
            charging: self.charging.tripped.unwrap_or(reading.charging),
            ..*reading
        }
    }

    /// Hysteresis applies only to recovery: trip at `threshold`, recover above `threshold + hyst`.
    fn trip(current: Option<bool>, pct: f64, threshold: f64, hysteresis: f64) -> bool {
        match current {
            Some(true) => pct <= threshold + hysteresis,
            _ => pct < threshold,
        }
    }
}

/// Runs hook scripts serially, so two hooks never race each other over the same services.
pub async fn run_hooks(cfg: HooksConfig, mut rx: mpsc::Receiver<(Event, Reading)>) {
    while let Some((event, reading)) = rx.recv().await {
        let Some(script) = event.script(&cfg) else {
            info!(event = event.as_str(), "no script configured, skipping");
            continue;
        };

        info!(
            event = event.as_str(),
            script = %script.display(),
            battery_pct = format!("{:.1}", reading.battery_pct),
            "running hook"
        );

        if let Err(e) = run_one(script, event, &reading, cfg.timeout_secs).await {
            // A broken hook must never take the daemon down: we still need to report and to run the
            // *other* hooks (a failed on_battery_low should not block on_battery_critical).
            error!(event = event.as_str(), script = %script.display(), "hook failed: {e:#}");
        }
    }
}

async fn run_one(script: &Path, event: Event, reading: &Reading, timeout_secs: u64) -> Result<()> {
    let mut child = tokio::process::Command::new(script)
        .env("UPS_EVENT", event.as_str())
        .env("UPS_BATTERY_PCT", format!("{:.2}", reading.battery_pct))
        .env("UPS_BUS_VOLTAGE", format!("{:.3}", reading.bus_voltage_v))
        .env("UPS_CURRENT_A", format!("{:.3}", reading.current_a))
        .env("UPS_POWER_W", format!("{:.3}", reading.power_w))
        .env("UPS_CHARGING", bool_env(reading.charging))
        .env("UPS_EXTERNAL_POWER", bool_env(reading.external_power))
        .spawn()
        .with_context(|| format!("spawning {}", script.display()))?;

    match tokio::time::timeout(Duration::from_secs(timeout_secs), child.wait()).await {
        Ok(status) => {
            let status = status.context("waiting for hook")?;
            if status.success() {
                info!(event = event.as_str(), "hook finished");
            } else {
                warn!(event = event.as_str(), ?status, "hook exited non-zero");
            }
        }
        Err(_) => {
            warn!(
                event = event.as_str(),
                timeout_secs, "hook timed out, killing"
            );
            let _ = child.kill().await;
        }
    }
    Ok(())
}

fn bool_env(v: bool) -> &'static str {
    if v {
        "1"
    } else {
        "0"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hooks_cfg() -> HooksConfig {
        toml::from_str("low_threshold_pct = 20.0\ncritical_threshold_pct = 5.0\nhysteresis_pct = 5.0\n")
            .unwrap()
    }

    fn reading(pct: f64) -> Reading {
        reading_with_power(pct, true)
    }

    fn reading_with_power(pct: f64, external_power: bool) -> Reading {
        Reading {
            bus_voltage_v: 11.0,
            current_a: if external_power { 0.5 } else { -0.5 },
            power_w: 5.5,
            compensated_voltage_v: 11.0,
            battery_pct: pct,
            charging: external_power,
            external_power,
        }
    }

    /// `charging` and `external_power` set independently: a pack idling on mains is neither
    /// charging nor discharging, which `reading_with_power` cannot express.
    fn reading_with(pct: f64, external_power: bool, charging: bool) -> Reading {
        Reading {
            charging,
            ..reading_with_power(pct, external_power)
        }
    }

    /// Feed a machine and collect everything it emits.
    fn drive(m: &mut HookMachine, pcts: &[f64]) -> Vec<Event> {
        pcts.iter().flat_map(|p| m.update(&reading(*p))).collect()
    }

    #[test]
    fn startup_latches_silently_by_default() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        // Booting at 2% must not fire anything with fire_on_startup = false.
        assert_eq!(m.update(&reading(2.0)), vec![]);
        // ...and having latched "low+critical", recovery still works normally.
        let events = drive(&mut m, &[80.0]);
        assert_eq!(events, vec![Event::BatteryCriticalClear, Event::BatteryOk]);
    }

    #[test]
    fn startup_fires_every_latch_when_configured() {
        // fire_on_startup means "sync the world to reality", so each latch reports its state --
        // including the nominal ones. Booting on mains at 2% emits PowerRestored as well as the
        // battery events, so a hook set that assumes symmetry stays consistent.
        let mut cfg = hooks_cfg();
        cfg.fire_on_startup = true;
        let mut m = HookMachine::new(&cfg, 1);
        assert_eq!(
            m.update(&reading_with_power(2.0, true)),
            vec![Event::PowerRestored, Event::BatteryLow, Event::BatteryCritical]
        );
    }

    #[test]
    fn startup_on_battery_fires_power_lost_when_configured() {
        let mut cfg = hooks_cfg();
        cfg.fire_on_startup = true;
        let mut m = HookMachine::new(&cfg, 1);
        assert_eq!(
            m.update(&reading_with_power(90.0, false)),
            vec![Event::PowerLost, Event::BatteryCriticalClear, Event::BatteryOk]
        );
    }

    #[test]
    fn crossing_low_fires_once_not_repeatedly() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading(50.0));

        assert_eq!(drive(&mut m, &[15.0]), vec![Event::BatteryLow]);
        // Still low: must not fire again on every tick.
        assert_eq!(drive(&mut m, &[14.0, 13.0, 12.0]), vec![]);
    }

    #[test]
    fn hysteresis_band_produces_no_events() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading(50.0));
        assert_eq!(drive(&mut m, &[15.0]), vec![Event::BatteryLow]);

        // 20-25% is the hysteresis band: above the threshold but not clear of it. Recovery must
        // wait for >25%.
        assert_eq!(drive(&mut m, &[21.0, 23.0, 25.0]), vec![]);
        assert_eq!(drive(&mut m, &[26.0]), vec![Event::BatteryOk]);
    }

    #[test]
    fn oscillating_across_the_threshold_fires_each_hook_exactly_once() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading(50.0));

        // A battery hovering right at 20% -- the flapping scenario hysteresis exists to prevent.
        let events = drive(&mut m, &[19.9, 20.1, 19.8, 20.2, 19.5, 20.4]);
        assert_eq!(
            events,
            vec![Event::BatteryLow],
            "hovering at the threshold must not thrash services"
        );
    }

    #[test]
    fn confirm_cycles_suppress_single_sample_spikes() {
        let mut m = HookMachine::new(&hooks_cfg(), 3);
        m.update(&reading(50.0));

        // One bad reading, then back to normal: nothing should fire.
        assert_eq!(drive(&mut m, &[1.0, 50.0, 50.0]), vec![]);
        // Three consecutive low readings: now it fires.
        assert_eq!(drive(&mut m, &[15.0, 15.0]), vec![]);
        assert_eq!(drive(&mut m, &[15.0]), vec![Event::BatteryLow]);
    }

    #[test]
    fn descending_fires_low_before_critical() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading(50.0));
        // Straight from healthy to critical in one step: stop services before the drastic action.
        assert_eq!(
            drive(&mut m, &[2.0]),
            vec![Event::BatteryLow, Event::BatteryCritical]
        );
    }

    #[test]
    fn ascending_clears_critical_before_ok() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading(50.0));
        drive(&mut m, &[2.0]);
        assert_eq!(
            drive(&mut m, &[90.0]),
            vec![Event::BatteryCriticalClear, Event::BatteryOk]
        );
    }

    #[test]
    fn power_events_lead_battery_events() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading_with_power(50.0, true));
        let events = m.update(&reading_with_power(2.0, false));
        assert_eq!(
            events,
            vec![Event::PowerLost, Event::BatteryLow, Event::BatteryCritical]
        );
    }

    #[test]
    fn power_loss_and_restore_fire_independently_of_battery_level() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading_with_power(90.0, true));

        assert_eq!(
            m.update(&reading_with_power(90.0, false)),
            vec![Event::PowerLost]
        );
        assert_eq!(m.update(&reading_with_power(89.0, false)), vec![]);
        assert_eq!(
            m.update(&reading_with_power(88.0, true)),
            vec![Event::PowerRestored]
        );
    }

    #[test]
    fn power_flapping_is_debounced() {
        let mut m = HookMachine::new(&hooks_cfg(), 3);
        m.update(&reading_with_power(90.0, true));

        // A single dropout must not fire.
        assert_eq!(m.update(&reading_with_power(90.0, false)), vec![]);
        assert_eq!(m.update(&reading_with_power(90.0, true)), vec![]);
        // Sustained loss does.
        assert_eq!(m.update(&reading_with_power(90.0, false)), vec![]);
        assert_eq!(m.update(&reading_with_power(90.0, false)), vec![]);
        assert_eq!(
            m.update(&reading_with_power(90.0, false)),
            vec![Event::PowerLost]
        );
    }

    #[test]
    fn critical_has_its_own_hysteresis_band() {
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading(50.0));
        drive(&mut m, &[2.0]);

        // 5-10% is critical's hysteresis band; still below low's 20% so no BatteryOk either.
        assert_eq!(drive(&mut m, &[8.0]), vec![]);
        assert_eq!(drive(&mut m, &[11.0]), vec![Event::BatteryCriticalClear]);
        assert_eq!(drive(&mut m, &[26.0]), vec![Event::BatteryOk]);
    }

    /// The bug this exists to prevent: a one-sample current transient past the deadband made
    /// `Reading::external_power` false for a single tick, and publishing that raw value showed a
    /// mains dropout in Home Assistant that the hooks had correctly debounced away.
    #[test]
    fn reported_power_ignores_a_single_sample_dropout_the_hooks_ignored() {
        let mut m = HookMachine::new(&hooks_cfg(), 3);
        m.update(&reading_with_power(90.0, true));

        let dropout = reading_with_power(90.0, false);
        assert_eq!(m.update(&dropout), vec![], "one sample must not fire a hook");
        assert!(
            m.reported(&dropout).external_power,
            "and must not be reported either -- the raw reading says false"
        );

        // Back to normal: the latch never moved, so neither does the reported state.
        let normal = reading_with_power(90.0, true);
        m.update(&normal);
        assert!(m.reported(&normal).external_power);
    }

    #[test]
    fn reported_power_follows_a_sustained_loss_once_confirmed() {
        let mut m = HookMachine::new(&hooks_cfg(), 3);
        m.update(&reading_with_power(90.0, true));

        let lost = reading_with_power(90.0, false);
        for _ in 0..2 {
            m.update(&lost);
            assert!(m.reported(&lost).external_power, "not confirmed yet");
        }
        assert_eq!(m.update(&lost), vec![Event::PowerLost]);
        assert!(
            !m.reported(&lost).external_power,
            "reported state must flip on the same tick the hook fires"
        );
    }

    #[test]
    fn reported_charging_is_debounced_the_same_way() {
        let mut m = HookMachine::new(&hooks_cfg(), 3);
        m.update(&reading_with(90.0, true, true));

        // A single tick where the charge current dips inside the deadband.
        let dip = reading_with(90.0, true, false);
        m.update(&dip);
        assert!(m.reported(&dip).charging, "one sample must not flap charging");

        // Sustained: the charger has genuinely tapered off.
        for _ in 0..2 {
            m.update(&dip);
        }
        assert!(!m.reported(&dip).charging);
    }

    #[test]
    fn reported_leaves_the_measurements_untouched() {
        // Only the two latched booleans are substituted; the numbers are real per-sample readings
        // and must reach MQTT exactly as measured.
        let mut m = HookMachine::new(&hooks_cfg(), 3);
        let r = reading_with_power(90.0, true);
        m.update(&r);

        let got = m.reported(&r);
        assert_eq!(got.bus_voltage_v, r.bus_voltage_v);
        assert_eq!(got.compensated_voltage_v, r.compensated_voltage_v);
        assert_eq!(got.current_a, r.current_a);
        assert_eq!(got.power_w, r.power_w);
        assert_eq!(got.battery_pct, r.battery_pct);
    }

    #[test]
    fn reported_passes_raw_values_through_before_the_first_update() {
        // Nothing latched yet, so there is no debounced view to offer.
        let m = HookMachine::new(&hooks_cfg(), 3);
        let r = reading_with(90.0, false, false);
        assert!(!m.reported(&r).external_power);
        assert!(!m.reported(&r).charging);
    }

    #[test]
    fn reported_tracks_the_latch_across_a_full_outage_and_recovery() {
        // The invariant that matters: what HA shows and what the hooks did never disagree.
        let mut m = HookMachine::new(&hooks_cfg(), 1);
        m.update(&reading_with_power(90.0, true));

        let lost = reading_with_power(90.0, false);
        assert_eq!(m.update(&lost), vec![Event::PowerLost]);
        assert!(!m.reported(&lost).external_power);

        let back = reading_with_power(90.0, true);
        assert_eq!(m.update(&back), vec![Event::PowerRestored]);
        assert!(m.reported(&back).external_power);
    }

    #[test]
    fn events_map_to_configured_scripts() {
        let cfg: HooksConfig = toml::from_str(
            "on_battery_low = \"/tmp/low.sh\"\non_power_restored = \"/tmp/restored.sh\"\n",
        )
        .unwrap();

        assert_eq!(
            Event::BatteryLow.script(&cfg).unwrap().to_str().unwrap(),
            "/tmp/low.sh"
        );
        assert_eq!(
            Event::PowerRestored.script(&cfg).unwrap().to_str().unwrap(),
            "/tmp/restored.sh"
        );
        // Unset hooks are simply skipped.
        assert!(Event::BatteryCritical.script(&cfg).is_none());
    }
}
