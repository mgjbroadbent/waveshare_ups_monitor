//! INA219 access, via the `ina219` crate.
//!
//! The crate owns the register map, config encoding and conversion-ready/overflow handling. We
//! supply a `Calibration` impl (the crate's intended extension point -- see its
//! `custom-calibration` example) carrying Waveshare's exact constants, and an app-level
//! `UpsSensor` trait so the Linux HAL can be target-gated and a simulated backend can stand in.

use anyhow::Result;
use async_trait::async_trait;
use ina219::calibration::Calibration;
use ina219::configuration::{
    BusVoltageRange, Configuration, MeasuredSignals, OperatingMode, Reset, Resolution,
    ShuntVoltageRange,
};
use ina219::measurements::{CurrentRegister, PowerRegister};

/// One set of readings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Sample {
    pub bus_voltage_v: f64,
    /// Positive = charging, negative = discharging. Verify against real hardware; if the shunt is
    /// wired the other way every sign flips (see README).
    pub current_a: f64,
    pub power_w: f64,
}

/// Waveshare UPS Module 3S calibration, transcribed from `INA219.py::set_calibration_16V_5A`.
///
/// Values come straight from the vendor file rather than `IntCalibration`, whose integer µA LSB
/// cannot represent 0.1524 mA (152.4 µA) and would emit cal=26946 against the vendor's 26868 --
/// equally accurate, but ~0.3% off when comparing side by side with INA219.py during bring-up.
///
/// Note the vendor's own comments disagree with its code: INA219.py:204 claims `Cal = 13434` while
/// :206 sets 26868. 26868 is the correct one -- it satisfies cal = 0.04096 / (lsb * r_shunt) for
/// lsb = 0.1524 mA and r_shunt = 0.01 Ohm.
///
/// Despite its name, `set_calibration_16V_5A` tops out at ~4.995A, not 5A: the current register is
/// signed 16-bit and wraps. Above that the chip raises its math-overflow flag and the crate errors
/// out, so we never report the wrapped value -- see the tests for the full story.
#[derive(Debug, Clone, Copy)]
pub struct WaveshareUps3S;

impl WaveshareUps3S {
    /// INA219.py:206
    const CAL_BITS: u16 = 26_868;
    /// INA219.py:200 -- mA per bit.
    const CURRENT_LSB_MA: f64 = 0.1524;
    /// INA219.py:211 -- W per bit (20 x current LSB).
    const POWER_LSB_W: f64 = 0.003048;
}

impl Calibration for WaveshareUps3S {
    type Current = f64; // amps
    type Power = f64; // watts

    fn register_bits(&self) -> u16 {
        Self::CAL_BITS
    }

    fn current_from_register(&self, reg: CurrentRegister) -> f64 {
        // Two's complement. The vendor's `if value > 32767: value -= 65535` is off by one LSB;
        // `as i16` is the correct conversion.
        (reg.0 as i16) as f64 * Self::CURRENT_LSB_MA / 1000.0
    }

    fn power_from_register(&self, reg: PowerRegister) -> f64 {
        // Unsigned: the power register is a magnitude and cannot be negative, so unlike the vendor
        // file we do not sign-convert it.
        f64::from(reg.0) * Self::POWER_LSB_W
    }
}

/// Matches `INA219.py:242-246`.
pub fn waveshare_configuration() -> Configuration {
    Configuration {
        reset: Reset::Run,
        bus_voltage_range: BusVoltageRange::Fsr16v,
        shunt_voltage_range: ShuntVoltageRange::Fsr80mv,
        bus_resolution: Resolution::Avg32,
        shunt_resolution: Resolution::Avg32,
        operating_mode: OperatingMode::Continous(MeasuredSignals::ShutAndBusVoltage),
    }
}

#[async_trait]
pub trait UpsSensor: Send {
    async fn read(&mut self) -> Result<Sample>;
}

/// Deterministic ramp for `--simulate`: exercises MQTT, discovery and the full hook chain on a dev
/// machine with no I2C present.
pub struct SimulatedSensor {
    voltage: f64,
    falling: bool,
    empty_v: f64,
    full_v: f64,
    step_v: f64,
}

impl SimulatedSensor {
    pub fn new(empty_v: f64, full_v: f64) -> Self {
        Self {
            voltage: full_v,
            falling: true,
            empty_v,
            full_v,
            // ~2% of the window per tick: crosses both thresholds within a short run.
            step_v: (full_v - empty_v) * 0.02,
        }
    }
}

#[async_trait]
impl UpsSensor for SimulatedSensor {
    async fn read(&mut self) -> Result<Sample> {
        // Ramp below empty and above full so clamping and both hook edges get exercised.
        let low = self.empty_v - 0.2;
        let high = self.full_v + 0.1;
        if self.falling {
            self.voltage -= self.step_v;
            if self.voltage <= low {
                self.voltage = low;
                self.falling = false;
            }
        } else {
            self.voltage += self.step_v;
            if self.voltage >= high {
                self.voltage = high;
                self.falling = true;
            }
        }

        // Discharging draws current (negative); charging pushes it back in (positive).
        let current_a = if self.falling { -0.65 } else { 0.9 };
        Ok(Sample {
            bus_voltage_v: self.voltage,
            current_a,
            power_w: self.voltage * current_a.abs(),
        })
    }
}

#[cfg(target_os = "linux")]
pub use linux::LinuxSensor;

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use anyhow::{Context, anyhow};
    use ina219::SyncIna219;
    use ina219::address::Address;
    use linux_embedded_hal::I2cdev;

    type Device = SyncIna219<I2cdev, WaveshareUps3S>;

    pub struct LinuxSensor {
        // Moved out into spawn_blocking on each read and moved back, so the blocking ioctl never
        // runs on a runtime worker. Only None if a previous read panicked mid-flight.
        device: Option<Device>,
    }

    impl LinuxSensor {
        pub fn new(bus: u8, address: u8) -> Result<Self> {
            let path = format!("/dev/i2c-{bus}");
            let dev = I2cdev::new(&path).with_context(|| format!("opening {path}"))?;
            let addr = Address::from_byte(address)
                .map_err(|e| anyhow!("invalid I2C address {address:#04x}: {e:?}"))?;

            let mut ina = SyncIna219::new_calibrated(dev, addr, WaveshareUps3S)
                .map_err(|e| anyhow!("initialising INA219 at {address:#04x} on {path}: {e:?}"))?;
            ina.set_configuration(waveshare_configuration())
                .map_err(|e| anyhow!("configuring INA219: {e:?}"))?;

            Ok(Self { device: Some(ina) })
        }
    }

    #[async_trait]
    impl UpsSensor for LinuxSensor {
        async fn read(&mut self) -> Result<Sample> {
            let mut dev = self
                .device
                .take()
                .ok_or_else(|| anyhow!("INA219 handle lost after an earlier failure"))?;

            // Move the device in and out: the ioctl is blocking, so it must not run inline.
            let (dev, result) = tokio::task::spawn_blocking(move || {
                let r = dev.next_measurement();
                (dev, r)
            })
            .await
            .context("INA219 read task panicked")?;

            self.device = Some(dev);

            match result {
                // None = conversion not ready yet; the caller retries on the next tick.
                Ok(None) => Err(anyhow!("conversion not ready")),
                Ok(Some(m)) => Ok(Sample {
                    bus_voltage_v: f64::from(m.bus_voltage.voltage_mv()) / 1000.0,
                    current_a: m.current,
                    power_w: m.power,
                }),
                Err(e) => Err(anyhow!("reading INA219: {e:?}")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ina219::calibration::simulate;
    use ina219::measurements::{BusVoltage, ShuntVoltage};

    // `simulate` runs the same register maths the chip does, so these need no hardware. Note it is
    // only usable for positive currents: it feeds the shunt register through `u32::from`, so a
    // two's-complement negative reads as a huge positive and spuriously overflows. Discharge is
    // covered by testing `current_from_register` directly below.
    #[test]
    fn calibration_converts_shunt_voltage_to_expected_current() {
        let bus = BusVoltage::from_mv(12_000);
        let shunt = ShuntVoltage::from_10uv(1_000); // 10mV over 0.01 Ohm -> 1.0A
        let m = simulate(&WaveshareUps3S, bus, shunt).expect("no overflow");

        assert!(
            (m.current - 1.0).abs() < 0.02,
            "expected ~1.0A, got {}",
            m.current
        );
    }

    #[test]
    fn calibration_handles_realistic_load_current() {
        // 3A x 0.01 Ohm = 30mV, comfortably inside the +/-80mV PGA range the config selects.
        let bus = BusVoltage::from_mv(12_600);
        let shunt = ShuntVoltage::from_10uv(3_000);
        let m = simulate(&WaveshareUps3S, bus, shunt).expect("no overflow");

        assert!(
            (m.current - 3.0).abs() < 0.02,
            "expected ~3.0A, got {}",
            m.current
        );
    }

    /// Waveshare's `set_calibration_16V_5A` is a misnomer: it cannot represent 5A.
    ///
    /// current_reg = shunt_10uv * 26868 / 4096, so the signed 16-bit register saturates at
    /// 32767 -> ~4.995A. At 5.0A the register reads 32797, which as i16 is -32739, i.e. a hefty
    /// *discharge*. (The vendor's own comment at INA219.py:215 claims a 3.2767A ceiling, which is
    /// stale copy-paste from set_calibration_32V_2A and wrong too.)
    ///
    /// Real hardware is protected: the chip raises its math-overflow flag and the crate's
    /// `next_measurement` turns that into `MeasurementError::MathOverflow` before any conversion,
    /// so `LinuxSensor` reports an error rather than a sign-flipped reading. That matters -- a
    /// wrapped value would look like discharging and fire the power-lost hooks. This test pins the
    /// ceiling so the limit is documented rather than rediscovered on hardware.
    #[test]
    fn current_register_wraps_negative_above_the_calibrations_real_ceiling() {
        assert!(
            (WaveshareUps3S.current_from_register(CurrentRegister(32_767)) - 4.9937).abs() < 0.001,
            "largest representable current should be ~4.99A"
        );
        // One LSB further wraps to full-scale negative.
        assert!(
            WaveshareUps3S.current_from_register(CurrentRegister(32_768)) < -4.9,
            "past the ceiling the register must read as a large negative, not a large positive"
        );
    }

    #[test]
    fn calibration_reports_discharge_as_negative_current() {
        // -1.0A -> reg -6559 (two's complement 0xE661). Tested directly rather than via
        // `simulate`, which cannot represent negative shunt voltages.
        let reg = CurrentRegister((-6559i16) as u16);
        let current = WaveshareUps3S.current_from_register(reg);

        assert!(
            (current + 1.0).abs() < 0.02,
            "expected ~-1.0A, got {current}"
        );
    }

    #[test]
    fn current_register_sign_conversion_is_exact_two_s_complement() {
        // The vendor's `if value > 32767: value -= 65535` is off by one LSB; `as i16` subtracts
        // 65536. Pin the boundary so a "faithful port" never reintroduces the vendor's bug.
        assert_eq!(
            WaveshareUps3S.current_from_register(CurrentRegister(0xFFFF)),
            -WaveshareUps3S::CURRENT_LSB_MA / 1000.0,
            "0xFFFF must be exactly -1 LSB"
        );
        assert_eq!(
            WaveshareUps3S.current_from_register(CurrentRegister(0)),
            0.0
        );
    }

    #[test]
    fn calibration_register_matches_vendor_file() {
        // Guards the vendor constant against the IntCalibration value (26946) we deliberately avoid.
        assert_eq!(WaveshareUps3S.register_bits(), 26_868);

        // cal = 0.04096 / (current_lsb_A * r_shunt_ohm), self-consistency check on the constants.
        let expected = 0.04096 / ((WaveshareUps3S::CURRENT_LSB_MA / 1000.0) * 0.01);
        assert!(
            (f64::from(WaveshareUps3S::CAL_BITS) - expected).abs() < 10.0,
            "cal {} inconsistent with LSB-derived {expected}",
            WaveshareUps3S::CAL_BITS
        );
    }

    #[test]
    fn power_lsb_is_twenty_times_current_lsb() {
        let expected = 20.0 * WaveshareUps3S::CURRENT_LSB_MA / 1000.0;
        assert!((WaveshareUps3S::POWER_LSB_W - expected).abs() < 1e-9);
    }

    #[tokio::test]
    async fn simulated_sensor_ramps_across_the_whole_window_and_reverses() {
        let mut s = SimulatedSensor::new(9.0, 12.6);
        let mut min = f64::MAX;
        let mut max = f64::MIN;
        let mut saw_charge = false;
        let mut saw_discharge = false;

        for _ in 0..400 {
            let sample = s.read().await.unwrap();
            min = min.min(sample.bus_voltage_v);
            max = max.max(sample.bus_voltage_v);
            saw_charge |= sample.current_a > 0.0;
            saw_discharge |= sample.current_a < 0.0;
        }

        assert!(min <= 9.0, "should ramp to/below empty, got {min}");
        assert!(max >= 12.6, "should ramp to/above full, got {max}");
        assert!(
            saw_charge && saw_discharge,
            "should exercise both directions"
        );
    }
}
