//! Configuration, loaded from TOML. Every section has defaults so a minimal config only needs
//! `[mqtt] host`.

use anyhow::{Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub i2c: I2cConfig,
    #[serde(default)]
    pub battery: BatteryConfig,
    pub mqtt: MqttConfig,
    #[serde(default)]
    pub monitor: MonitorConfig,
    #[serde(default)]
    pub hooks: HooksConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct I2cConfig {
    /// I2C bus number, i.e. /dev/i2c-<bus>.
    #[serde(default = "default_bus")]
    pub bus: u8,
    /// INA219 address. 0x41 on the Waveshare UPS Module 3S.
    #[serde(default = "default_address")]
    pub address: u8,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BatteryConfig {
    /// Cells in series. 3 for the UPS Module 3S.
    #[serde(default = "default_cells")]
    pub cells: f64,
    /// Per-cell voltage treated as 0%. 3.0 x 3 cells = 9.0V, matching INA219.py:287.
    #[serde(default = "default_empty_vpc")]
    pub empty_volts_per_cell: f64,
    /// Per-cell voltage treated as 100%. 4.2 x 3 cells = 12.6V.
    #[serde(default = "default_full_vpc")]
    pub full_volts_per_cell: f64,
    /// Pack internal resistance, used to recover open-circuit voltage from the loaded bus voltage.
    /// The one value that genuinely needs tuning per pack -- see README.
    #[serde(default = "default_r_internal")]
    pub internal_resistance_ohms: f64,
    /// EMA smoothing factor for the compensated voltage. 1.0 = no smoothing, 0 = frozen.
    #[serde(default = "default_ema_alpha")]
    pub ema_alpha: f64,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_base_topic")]
    pub base_topic: String,
    #[serde(default = "default_discovery_prefix")]
    pub discovery_prefix: String,
    /// Defaults to the hostname. Used in topics and entity unique_ids.
    pub device_id: Option<String>,
    #[serde(default = "default_keep_alive")]
    pub keep_alive_secs: u64,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MonitorConfig {
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Above this, we are charging.
    #[serde(default = "default_charge_threshold")]
    pub charging_threshold_ma: f64,
    /// Below negative this, we are discharging (and therefore on battery).
    #[serde(default = "default_charge_threshold")]
    pub discharging_threshold_ma: f64,
    /// Consecutive readings required before a hook state change is accepted.
    #[serde(default = "default_confirm_cycles")]
    pub confirm_cycles: u32,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct HooksConfig {
    #[serde(default = "default_low_threshold")]
    pub low_threshold_pct: f64,
    #[serde(default = "default_critical_threshold")]
    pub critical_threshold_pct: f64,
    /// Recovery requires crossing back above `threshold + hysteresis`, so a battery sitting on the
    /// threshold does not flap services up and down.
    #[serde(default = "default_hysteresis")]
    pub hysteresis_pct: f64,
    #[serde(default = "default_hook_timeout")]
    pub timeout_secs: u64,
    /// When false, the first reading latches state silently: restarting the daemon will not tear
    /// down services that are already in the right state.
    ///
    /// When true, every latch fires on the first reading to sync the world to reality -- including
    /// the nominal ones, so booting on mains at a healthy charge runs `on_power_restored` and
    /// `on_battery_ok`. Hook scripts must therefore be idempotent.
    #[serde(default)]
    pub fire_on_startup: bool,

    pub on_battery_low: Option<PathBuf>,
    pub on_battery_ok: Option<PathBuf>,
    pub on_battery_critical: Option<PathBuf>,
    pub on_battery_critical_clear: Option<PathBuf>,
    pub on_power_lost: Option<PathBuf>,
    pub on_power_restored: Option<PathBuf>,
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let config: Config = toml::from_str(&raw)
            .with_context(|| format!("parsing config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    /// Catch configs that parse but cannot work, at startup rather than at 3am on battery.
    fn validate(&self) -> Result<()> {
        let b = &self.battery;
        anyhow::ensure!(b.cells > 0.0, "battery.cells must be > 0");
        anyhow::ensure!(
            b.full_volts_per_cell > b.empty_volts_per_cell,
            "battery.full_volts_per_cell ({}) must exceed empty_volts_per_cell ({})",
            b.full_volts_per_cell,
            b.empty_volts_per_cell
        );
        anyhow::ensure!(
            b.internal_resistance_ohms >= 0.0,
            "battery.internal_resistance_ohms must be >= 0"
        );
        anyhow::ensure!(
            b.ema_alpha > 0.0 && b.ema_alpha <= 1.0,
            "battery.ema_alpha must be in (0, 1]"
        );
        anyhow::ensure!(!self.mqtt.host.is_empty(), "mqtt.host must be set");
        if let Some(id) = &self.mqtt.device_id {
            // Better to fail at startup than to publish under a name the user never chose.
            anyhow::ensure!(
                !slugify(id).is_empty(),
                "mqtt.device_id ({id:?}) contains no characters usable in an MQTT discovery topic; \
                 it must have at least one of [a-zA-Z0-9_]"
            );
        }
        anyhow::ensure!(
            self.monitor.poll_interval_secs > 0,
            "monitor.poll_interval_secs must be > 0"
        );

        let h = &self.hooks;
        anyhow::ensure!(
            h.critical_threshold_pct <= h.low_threshold_pct,
            "hooks.critical_threshold_pct ({}) must not exceed low_threshold_pct ({})",
            h.critical_threshold_pct,
            h.low_threshold_pct
        );
        anyhow::ensure!(h.hysteresis_pct >= 0.0, "hooks.hysteresis_pct must be >= 0");
        Ok(())
    }

    /// Slugified machine identity: the configured `mqtt.device_id`, else the hostname.
    ///
    /// Slugified because it feeds the `node_id` level of the discovery topic, and Home Assistant
    /// requires `[a-zA-Z0-9_-]` there -- a hostname like `raspberrypi.local` would otherwise be
    /// rejected outright, and the entity would silently never appear.
    pub fn device_id(&self) -> String {
        let raw = self
            .mqtt
            .device_id
            .clone()
            .unwrap_or_else(|| gethostname::gethostname().to_string_lossy().to_string());

        let slug = slugify(&raw);
        if slug.is_empty() {
            // Only reachable via a hostname with nothing slug-worthy in it; an explicit device_id
            // that slugs to nothing is rejected by validate() instead of silently landing here.
            tracing::warn!("hostname {raw:?} has no usable characters; falling back to \"ups\"");
            return "ups".to_string();
        }
        slug
    }

    pub fn empty_volts(&self) -> f64 {
        self.battery.empty_volts_per_cell * self.battery.cells
    }

    pub fn full_volts(&self) -> f64 {
        self.battery.full_volts_per_cell * self.battery.cells
    }
}

/// Conform a string to Home Assistant's `[a-zA-Z0-9_-]` requirement for the `node_id` and
/// `object_id` levels of a discovery topic:
///
/// ```text
/// <discovery_prefix>/<component>/[<node_id>/]<object_id>/config
/// ```
///
/// Anything outside the class becomes `-`, runs collapse, and leading/trailing `-` are trimmed.
///
/// Deliberately does not truncate at the first dot, tempting though `raspberrypi.local` ->
/// `raspberrypi` looks: `pi.a.example` and `pi.b.example` would both collapse to `pi` and collide,
/// which is precisely what this naming scheme exists to prevent. Slugifying whole keeps every host
/// distinct.
///
/// Case is preserved, since the character class permits `A-Z`.
fn slugify(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') {
            // Collapses runs: `a...b` -> `a-b`, not `a---b`.
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

fn default_bus() -> u8 {
    1
}
fn default_address() -> u8 {
    0x41
}
fn default_cells() -> f64 {
    3.0
}
fn default_empty_vpc() -> f64 {
    3.0
}
fn default_full_vpc() -> f64 {
    4.2
}
fn default_r_internal() -> f64 {
    0.20
}
fn default_ema_alpha() -> f64 {
    0.2
}
fn default_mqtt_port() -> u16 {
    1883
}
fn default_base_topic() -> String {
    "waveshare-ups".to_string()
}
fn default_discovery_prefix() -> String {
    "homeassistant".to_string()
}
fn default_keep_alive() -> u64 {
    30
}
fn default_poll_interval() -> u64 {
    2
}
fn default_charge_threshold() -> f64 {
    50.0
}
fn default_confirm_cycles() -> u32 {
    3
}
fn default_low_threshold() -> f64 {
    20.0
}
fn default_critical_threshold() -> f64 {
    5.0
}
fn default_hysteresis() -> f64 {
    5.0
}
fn default_hook_timeout() -> u64 {
    60
}

impl Default for I2cConfig {
    fn default() -> Self {
        Self {
            bus: default_bus(),
            address: default_address(),
        }
    }
}

impl Default for BatteryConfig {
    fn default() -> Self {
        Self {
            cells: default_cells(),
            empty_volts_per_cell: default_empty_vpc(),
            full_volts_per_cell: default_full_vpc(),
            internal_resistance_ohms: default_r_internal(),
            ema_alpha: default_ema_alpha(),
        }
    }
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            poll_interval_secs: default_poll_interval(),
            charging_threshold_ma: default_charge_threshold(),
            discharging_threshold_ma: default_charge_threshold(),
            confirm_cycles: default_confirm_cycles(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<Config> {
        let c: Config = toml::from_str(s)?;
        c.validate()?;
        Ok(c)
    }

    #[test]
    fn minimal_config_gets_waveshare_3s_defaults() {
        let c = parse("[mqtt]\nhost = \"broker\"\n").unwrap();
        assert_eq!(c.i2c.address, 0x41);
        assert_eq!(c.empty_volts(), 9.0);
        assert!((c.full_volts() - 12.6).abs() < 1e-9); // 4.2 * 3.0 is not exactly 12.6
        assert_eq!(c.monitor.poll_interval_secs, 2);
        assert!(!c.hooks.fire_on_startup);
    }

    #[test]
    fn cells_scale_the_voltage_window() {
        // A 2S pack should land on the ugursayar window without any other changes.
        let c = parse("[mqtt]\nhost = \"b\"\n[battery]\ncells = 2.0\n").unwrap();
        assert_eq!(c.empty_volts(), 6.0);
        assert!((c.full_volts() - 8.4).abs() < 1e-9);
    }

    #[test]
    fn rejects_inverted_voltage_window() {
        let err = parse("[mqtt]\nhost = \"b\"\n[battery]\nempty_volts_per_cell = 4.2\nfull_volts_per_cell = 3.0\n")
            .unwrap_err();
        assert!(err.to_string().contains("must exceed"));
    }

    #[test]
    fn rejects_critical_above_low() {
        let err = parse("[mqtt]\nhost = \"b\"\n[hooks]\nlow_threshold_pct = 10.0\ncritical_threshold_pct = 50.0\n")
            .unwrap_err();
        assert!(err.to_string().contains("must not exceed"));
    }

    #[test]
    fn rejects_typo_keys_rather_than_silently_ignoring() {
        // deny_unknown_fields: a misspelled threshold must not silently keep the default.
        assert!(parse("[mqtt]\nhost = \"b\"\n[hooks]\nlow_threshold = 20.0\n").is_err());
    }

    /// Home Assistant requires [a-zA-Z0-9_-] for the node_id and object_id levels of a discovery
    /// topic and rejects the config outright otherwise -- the entity simply never appears, with no
    /// obvious error. A dotted hostname is the common way to trip this.
    #[test]
    fn slugify_strips_characters_home_assistant_rejects() {
        assert_eq!(slugify("raspberrypi.local"), "raspberrypi-local");
        assert_eq!(slugify("pi.home.arpa"), "pi-home-arpa");
        assert_eq!(slugify("Marks-MacBook-Pro.local"), "Marks-MacBook-Pro-local");
    }

    #[test]
    fn slugify_leaves_already_valid_names_untouched() {
        assert_eq!(slugify("raspberrypi"), "raspberrypi");
        assert_eq!(slugify("pi_2"), "pi_2");
        assert_eq!(slugify("ups-01"), "ups-01");
        assert_eq!(slugify("UPPER_case-99"), "UPPER_case-99");
    }

    #[test]
    fn slugify_collapses_runs_and_trims_edges() {
        assert_eq!(slugify("a...b"), "a-b");
        assert_eq!(slugify("--pi--"), "pi");
        assert_eq!(slugify(".pi."), "pi");
        assert_eq!(slugify("a b  c"), "a-b-c");
    }

    #[test]
    fn slugify_handles_non_ascii_and_topic_wildcards() {
        // MQTT wildcards and separators would break topic structure; non-ASCII is outside the class.
        assert_eq!(slugify("pi/2+3#4"), "pi-2-3-4");
        assert_eq!(slugify("café"), "caf");
        assert_eq!(slugify("日本"), "");
    }

    #[test]
    fn slugify_output_always_conforms_to_the_home_assistant_class() {
        for input in [
            "raspberrypi.local",
            "pi/2+3#4",
            "café",
            "a...b",
            "--x--",
            "UPPER.case",
            "1.2.3.4",
        ] {
            let s = slugify(input);
            assert!(
                s.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "slugify({input:?}) = {s:?} escaped the character class"
            );
        }
    }

    #[test]
    fn slugify_does_not_truncate_at_dots_so_hosts_stay_distinct() {
        // Truncating to the short hostname would collapse these two into the same id.
        assert_ne!(slugify("pi.a.example"), slugify("pi.b.example"));
    }

    #[test]
    fn device_id_slugifies_an_explicitly_configured_value() {
        // Previously the configured value bypassed sanitising entirely.
        let c = parse("[mqtt]\nhost = \"b\"\ndevice_id = \"shed.ups\"\n").unwrap();
        assert_eq!(c.device_id(), "shed-ups");
    }

    #[test]
    fn rejects_a_device_id_with_nothing_sluggable_in_it() {
        let err = parse("[mqtt]\nhost = \"b\"\ndevice_id = \"...\"\n").unwrap_err();
        assert!(err.to_string().contains("no characters usable"));
    }

    #[test]
    fn device_id_from_hostname_is_always_valid() {
        let c = parse("[mqtt]\nhost = \"b\"\n").unwrap();
        let id = c.device_id();
        assert!(!id.is_empty());
        assert!(
            id.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "hostname-derived device_id {id:?} is not discovery-safe"
        );
    }
}
