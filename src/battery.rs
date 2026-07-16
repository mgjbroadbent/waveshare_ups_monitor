//! State of charge, and the charge/discharge/mains determination.
//!
//! # Why one formula and not two
//!
//! The brief asked for the `INA219.py:287` formula while charging and the ugursayar
//! `ups_shutdown.py:128` formula while discharging. Those are the same formula at different cell
//! counts -- both interpolate 3.0 V/cell (empty) to 4.2 V/cell (full); the first is 3S (9.0-12.6V),
//! the second 2S (6.0-8.4V, per that script's own comment: "3.20 V / cell on 2S pack"). Applied
//! literally to one pack they disagree wildly (at 11.0V on 3S: 55.6% charging vs 100% discharging),
//! so the percentage would jump at every charge/discharge transition.
//!
//! The reason charge and discharge genuinely differ is that a pack reads high under charge and sags
//! under load. IR compensation attacks that cause directly, and since charge current is positive and
//! discharge negative, a single expression covers both directions.

use crate::config::Config;
use crate::sensor::Sample;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub bus_voltage_v: f64,
    pub current_a: f64,
    pub power_w: f64,
    /// Bus voltage corrected back to open-circuit, smoothed.
    pub compensated_voltage_v: f64,
    pub battery_pct: f64,
    pub charging: bool,
    /// True unless actively discharging -- a full pack on mains draws ~no current, so "is charging"
    /// is not a usable test for mains presence. See `Monitor::evaluate`.
    pub external_power: bool,
}

pub struct Monitor {
    empty_v: f64,
    full_v: f64,
    r_internal: f64,
    ema_alpha: f64,
    charging_threshold_a: f64,
    discharging_threshold_a: f64,
    /// Smoothed compensated voltage; None until the first reading seeds it.
    ema_v: Option<f64>,
}

impl Monitor {
    pub fn new(config: &Config) -> Self {
        Self {
            empty_v: config.empty_volts(),
            full_v: config.full_volts(),
            r_internal: config.battery.internal_resistance_ohms,
            ema_alpha: config.battery.ema_alpha,
            charging_threshold_a: config.monitor.charging_threshold_ma / 1000.0,
            discharging_threshold_a: config.monitor.discharging_threshold_ma / 1000.0,
            ema_v: None,
        }
    }

    pub fn evaluate(&mut self, sample: Sample) -> Reading {
        // Recover open-circuit voltage. Charging (I > 0) inflates the bus voltage, so we subtract;
        // discharging (I < 0) depresses it, and subtracting a negative adds it back. One expression,
        // both directions.
        let v_oc = sample.bus_voltage_v - (sample.current_a * self.r_internal);

        let smoothed = match self.ema_v {
            Some(prev) => self.ema_alpha * v_oc + (1.0 - self.ema_alpha) * prev,
            // Seed with the first real reading rather than ramping up from zero, which would
            // otherwise read as a flat battery for the first several ticks and fire the hooks.
            None => v_oc,
        };
        self.ema_v = Some(smoothed);

        // INA219.py:287, applied to the compensated voltage.
        let battery_pct =
            ((smoothed - self.empty_v) / (self.full_v - self.empty_v) * 100.0).clamp(0.0, 100.0);

        let charging = sample.current_a > self.charging_threshold_a;
        let discharging = sample.current_a < -self.discharging_threshold_a;

        Reading {
            bus_voltage_v: sample.bus_voltage_v,
            current_a: sample.current_a,
            power_w: sample.power_w,
            compensated_voltage_v: smoothed,
            battery_pct,
            charging,
            // Mains is present unless we are actively pulling from the battery. Testing "charging"
            // instead would report mains-lost on a full pack, whose charge current tapers to ~0.
            external_power: !discharging,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config() -> Config {
        toml::from_str("[mqtt]\nhost = \"b\"\n").unwrap()
    }

    /// No smoothing and no IR correction: isolates the raw SoC curve.
    fn bare_monitor() -> Monitor {
        let mut c = config();
        c.battery.ema_alpha = 1.0;
        c.battery.internal_resistance_ohms = 0.0;
        Monitor::new(&c)
    }

    fn sample(v: f64, a: f64) -> Sample {
        Sample {
            bus_voltage_v: v,
            current_a: a,
            power_w: v * a.abs(),
        }
    }

    #[test]
    fn soc_matches_the_vendor_curve_at_both_rails_and_midpoint() {
        let mut m = bare_monitor();
        assert!((m.evaluate(sample(9.0, 0.0)).battery_pct - 0.0).abs() < 1e-6);

        let mut m = bare_monitor();
        assert!((m.evaluate(sample(10.8, 0.0)).battery_pct - 50.0).abs() < 1e-6);

        let mut m = bare_monitor();
        assert!((m.evaluate(sample(12.6, 0.0)).battery_pct - 100.0).abs() < 1e-6);
    }

    #[test]
    fn soc_agrees_with_ina219_py_line_287() {
        // Cross-check against the vendor expression itself, over the whole range.
        for mv in (8_000..=13_500).step_by(100) {
            let v = f64::from(mv) / 1000.0;
            let expected = ((v - 9.0) / 3.6 * 100.0).clamp(0.0, 100.0);
            let got = bare_monitor().evaluate(sample(v, 0.0)).battery_pct;
            assert!(
                (got - expected).abs() < 1e-6,
                "at {v}V: {got} vs {expected}"
            );
        }
    }

    #[test]
    fn soc_clamps_outside_the_window() {
        assert_eq!(bare_monitor().evaluate(sample(5.0, 0.0)).battery_pct, 0.0);
        assert_eq!(
            bare_monitor().evaluate(sample(20.0, 0.0)).battery_pct,
            100.0
        );
    }

    #[test]
    fn ir_compensation_pulls_charging_voltage_down() {
        let mut c = config();
        c.battery.ema_alpha = 1.0;
        c.battery.internal_resistance_ohms = 0.2;
        let mut m = Monitor::new(&c);

        // Charging at +1A: the charger inflates the bus voltage, so true OC is 0.2V lower.
        let r = m.evaluate(sample(11.0, 1.0));
        assert!(
            (r.compensated_voltage_v - 10.8).abs() < 1e-9,
            "got {}",
            r.compensated_voltage_v
        );
        assert!(r.compensated_voltage_v < 11.0, "charging must read lower");
    }

    #[test]
    fn ir_compensation_pushes_discharging_voltage_up() {
        let mut c = config();
        c.battery.ema_alpha = 1.0;
        c.battery.internal_resistance_ohms = 0.2;
        let mut m = Monitor::new(&c);

        // Discharging at -1A: load sags the bus voltage, so true OC is 0.2V higher.
        let r = m.evaluate(sample(11.0, -1.0));
        assert!(
            (r.compensated_voltage_v - 11.2).abs() < 1e-9,
            "got {}",
            r.compensated_voltage_v
        );
        assert!(
            r.compensated_voltage_v > 11.0,
            "discharging must read higher"
        );
    }

    #[test]
    fn ir_compensation_closes_the_gap_across_a_charge_transition() {
        // The point of the whole exercise: same pack, same instant, load switching from -1A draw to
        // +1A charge should not move the reported percentage much. Uncompensated it jumps ~11%.
        let mut c = config();
        c.battery.ema_alpha = 1.0;
        c.battery.internal_resistance_ohms = 0.2;

        // A 0.2 Ohm pack at 10.8V OC reads 10.6V under 1A load, 11.0V under 1A charge.
        let discharging = Monitor::new(&c).evaluate(sample(10.6, -1.0)).battery_pct;
        let charging = Monitor::new(&c).evaluate(sample(11.0, 1.0)).battery_pct;
        assert!(
            (discharging - charging).abs() < 0.5,
            "compensated SoC should barely move: {discharging} vs {charging}"
        );

        // Without compensation the same pair differs by an order of magnitude more.
        c.battery.internal_resistance_ohms = 0.0;
        let raw_dis = Monitor::new(&c).evaluate(sample(10.6, -1.0)).battery_pct;
        let raw_chg = Monitor::new(&c).evaluate(sample(11.0, 1.0)).battery_pct;
        assert!(
            (raw_dis - raw_chg).abs() > 10.0,
            "uncompensated should show the jump this exists to remove"
        );
    }

    #[test]
    fn full_pack_on_mains_at_idle_current_still_reports_external_power() {
        // The failure the "not discharging" test exists to prevent: a full pack's charge current
        // tapers to ~0, and `current > 50mA` would call that mains-lost and fire the hooks.
        let r = bare_monitor().evaluate(sample(12.6, 0.001));
        assert!(!r.charging, "1mA is below the charge threshold");
        assert!(
            r.external_power,
            "must not report mains lost when merely idle"
        );
    }

    #[test]
    fn discharging_reports_mains_lost() {
        let r = bare_monitor().evaluate(sample(11.5, -0.6));
        assert!(!r.charging);
        assert!(!r.external_power);
    }

    #[test]
    fn charging_reports_both_charging_and_external_power() {
        let r = bare_monitor().evaluate(sample(11.5, 0.9));
        assert!(r.charging);
        assert!(r.external_power);
    }

    #[test]
    fn small_currents_inside_the_deadband_are_neither_charging_nor_discharging() {
        // +50/-200mA default deadband keeps sensor noise from flapping the state.
        let r = bare_monitor().evaluate(sample(11.5, -0.02));
        assert!(!r.charging);
        assert!(r.external_power, "noise must not read as mains lost");
    }

    /// The deadband is asymmetric for a measured reason, so pin both sides of the gap it straddles.
    /// A full pack on mains briefly hands the load back to the pack; a symmetric +/-50mA deadband
    /// called that mains loss and fired the power hooks. See README.
    #[test]
    fn the_default_deadband_spans_the_gap_between_an_idle_excursion_and_a_real_loss() {
        // Worst observed idle excursion: -177mA, ~1W, pack voltage flat.
        let r = bare_monitor().evaluate(sample(12.44, -0.177));
        assert!(
            r.external_power,
            "the idle excursion must not read as mains lost"
        );

        // Weakest observed genuine loss: -360mA, the Pi's whole draw on the pack.
        let r = bare_monitor().evaluate(sample(12.388, -0.360));
        assert!(!r.external_power, "a real dropout must still be detected");
    }

    #[test]
    fn ema_seeds_from_first_reading_rather_than_ramping_from_zero() {
        let mut c = config();
        c.battery.ema_alpha = 0.2; // heavy smoothing
        c.battery.internal_resistance_ohms = 0.0;
        let mut m = Monitor::new(&c);

        // Seeding from 0.0 would report ~0% on the first tick and trip the critical hook on startup.
        let first = m.evaluate(sample(12.6, 0.0));
        assert!(
            (first.battery_pct - 100.0).abs() < 1e-6,
            "first reading must be taken at face value, got {}",
            first.battery_pct
        );
    }

    #[test]
    fn ema_smooths_subsequent_readings_towards_the_new_value() {
        let mut c = config();
        c.battery.ema_alpha = 0.5;
        c.battery.internal_resistance_ohms = 0.0;
        let mut m = Monitor::new(&c);

        m.evaluate(sample(12.6, 0.0));
        // A sudden drop should be damped, not followed instantly.
        let second = m.evaluate(sample(9.0, 0.0));
        assert!(
            (second.compensated_voltage_v - 10.8).abs() < 1e-9,
            "alpha 0.5 should land halfway, got {}",
            second.compensated_voltage_v
        );
    }

    #[test]
    fn a_single_spike_cannot_swing_smoothed_voltage_far() {
        let mut c = config();
        c.battery.ema_alpha = 0.2;
        c.battery.internal_resistance_ohms = 0.0;
        let mut m = Monitor::new(&c);

        for _ in 0..20 {
            m.evaluate(sample(12.0, 0.0));
        }
        let spiked = m.evaluate(sample(0.0, 0.0)).compensated_voltage_v;
        assert!(
            spiked > 9.0,
            "one bad sample must not read as empty: {spiked}"
        );
    }
}
