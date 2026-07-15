//! MQTT publishing and Home Assistant autodiscovery.
//!
//! Availability is driven by an MQTT last will: the broker publishes `offline` on our behalf if we
//! drop without a clean disconnect, and every discovery payload points at that one topic, so all
//! entities go unavailable together.

use crate::battery::Reading;
use crate::config::Config;
use anyhow::Result;
use rumqttc::{AsyncClient, LastWill, MqttOptions, QoS};
use serde::Serialize;
use serde_json::json;
use std::time::Duration;

pub const ONLINE: &str = "online";
pub const OFFLINE: &str = "offline";

/// Namespace for everything that shares a flat global keyspace with other MQTT publishers: the
/// discovery topic's node id and entity `unique_id`s. A bare hostname is a poor key there -- another
/// integration on the same host publishing `homeassistant/sensor/<hostname>/battery/config` would
/// land on the identical topic and silently clobber ours.
const NODE_PREFIX: &str = "waveshare-ups";

/// Topics and identity derived from config, shared by the sampler and the event loop.
#[derive(Debug, Clone)]
pub struct Topics {
    /// Plain machine identity (hostname unless overridden). Names the device in HA and namespaces
    /// our own topics, both of which already sit under a `waveshare-ups` prefix.
    pub device_id: String,
    /// `waveshare-ups-<device_id>`. Used only where the key is globally shared -- see NODE_PREFIX.
    pub node_id: String,
    pub state: String,
    pub availability: String,
    discovery_prefix: String,
}

impl Topics {
    pub fn new(config: &Config) -> Self {
        let device_id = config.device_id();
        let base = &config.mqtt.base_topic;
        Self {
            // Our own topics are already namespaced by base_topic, so they keep the bare device_id
            // rather than repeating the prefix: `waveshare-ups/pi/state`, not
            // `waveshare-ups/waveshare-ups-pi/state`.
            state: format!("{base}/{device_id}/state"),
            availability: format!("{base}/{device_id}/availability"),
            discovery_prefix: config.mqtt.discovery_prefix.clone(),
            node_id: format!("{NODE_PREFIX}-{device_id}"),
            device_id,
        }
    }

    fn discovery(&self, component: &str, object: &str) -> String {
        format!(
            "{}/{component}/{}/{object}/config",
            self.discovery_prefix, self.node_id
        )
    }
}

/// The single JSON payload published each cycle; discovery templates pull fields out of it.
#[derive(Debug, Serialize)]
pub struct StatePayload {
    pub bus_voltage: f64,
    pub current: f64,
    pub power: f64,
    pub battery: f64,
    pub compensated_voltage: f64,
    pub charging: bool,
    pub external_power: bool,
}

impl From<&Reading> for StatePayload {
    fn from(r: &Reading) -> Self {
        // Round for readability in HA; the underlying precision is well below this anyway.
        Self {
            bus_voltage: round(r.bus_voltage_v, 3),
            current: round(r.current_a, 3),
            power: round(r.power_w, 3),
            battery: round(r.battery_pct, 1),
            compensated_voltage: round(r.compensated_voltage_v, 3),
            charging: r.charging,
            external_power: r.external_power,
        }
    }
}

fn round(v: f64, places: u32) -> f64 {
    let f = 10f64.powi(places as i32);
    (v * f).round() / f
}

pub fn build_client(config: &Config, topics: &Topics) -> (AsyncClient, rumqttc::EventLoop) {
    // Same shape and same reason: the broker's client-id keyspace is shared with everything else
    // connected to it.
    let mut opts = MqttOptions::new(&topics.node_id, &config.mqtt.host, config.mqtt.port);
    opts.set_keep_alive(Duration::from_secs(config.mqtt.keep_alive_secs));

    if let (Some(u), Some(p)) = (&config.mqtt.username, &config.mqtt.password) {
        opts.set_credentials(u.clone(), p.clone());
    }

    // The broker publishes this if we vanish without a clean disconnect -- crash, power cut, network
    // drop. Retained, so HA sees it even if it subscribes later.
    opts.set_last_will(LastWill::new(
        &topics.availability,
        OFFLINE,
        QoS::AtLeastOnce,
        true,
    ));

    AsyncClient::new(opts, 32)
}

/// A HA discovery entity. `object` is the id fragment; `component` is HA's platform.
struct Entity {
    component: &'static str,
    object: &'static str,
    name: &'static str,
    device_class: &'static str,
    unit: Option<&'static str>,
    /// Jinja over the state JSON.
    value_template: &'static str,
}

/// The four required values, plus the two booleans the power hooks key off -- publishing them costs
/// nothing and makes the hook behaviour visible in HA.
const ENTITIES: &[Entity] = &[
    Entity {
        component: "sensor",
        object: "bus_voltage",
        name: "Bus voltage",
        device_class: "voltage",
        unit: Some("V"),
        value_template: "{{ value_json.bus_voltage }}",
    },
    Entity {
        component: "sensor",
        object: "current",
        name: "Current",
        device_class: "current",
        unit: Some("A"),
        value_template: "{{ value_json.current }}",
    },
    Entity {
        component: "sensor",
        object: "power",
        name: "Power",
        device_class: "power",
        unit: Some("W"),
        value_template: "{{ value_json.power }}",
    },
    Entity {
        component: "sensor",
        object: "battery",
        name: "Battery",
        device_class: "battery",
        unit: Some("%"),
        value_template: "{{ value_json.battery }}",
    },
    Entity {
        component: "binary_sensor",
        object: "external_power",
        name: "External power",
        device_class: "plug",
        unit: None,
        value_template: "{{ 'ON' if value_json.external_power else 'OFF' }}",
    },
    Entity {
        component: "binary_sensor",
        object: "charging",
        name: "Charging",
        device_class: "battery_charging",
        unit: None,
        value_template: "{{ 'ON' if value_json.charging else 'OFF' }}",
    },
];

fn discovery_payload(entity: &Entity, topics: &Topics) -> serde_json::Value {
    let mut payload = json!({
        // HA prefixes the device name automatically, so this stays bare.
        "name": entity.name,
        // Prefixed: unique_id shares one keyspace across every MQTT-discovered entity in HA, so a
        // bare `<hostname>_battery` is asking for a collision.
        "unique_id": format!("{}_{}", topics.node_id, entity.object),
        "state_topic": topics.state,
        "value_template": entity.value_template,
        "device_class": entity.device_class,
        "availability_topic": topics.availability,
        "payload_available": ONLINE,
        "payload_not_available": OFFLINE,
        "device": {
            "identifiers": [format!("waveshare_ups_{}", topics.device_id)],
            "name": format!("Waveshare UPS ({})", topics.device_id),
            "manufacturer": "Waveshare",
            "model": "UPS Module 3S",
        }
    });

    if let Some(unit) = entity.unit {
        payload["unit_of_measurement"] = json!(unit);
        // Numeric readings only: `state_class` on a binary_sensor is invalid.
        payload["state_class"] = json!("measurement");
    } else {
        payload["payload_on"] = json!("ON");
        payload["payload_off"] = json!("OFF");
    }

    payload
}

/// Retained, so HA repopulates entities from the broker after a restart without waiting for us.
pub async fn publish_discovery(client: &AsyncClient, topics: &Topics) -> Result<()> {
    for entity in ENTITIES {
        let topic = topics.discovery(entity.component, entity.object);
        let payload = serde_json::to_vec(&discovery_payload(entity, topics))?;
        client
            .publish(topic, QoS::AtLeastOnce, true, payload)
            .await?;
    }
    Ok(())
}

pub async fn publish_availability(client: &AsyncClient, topics: &Topics, value: &str) -> Result<()> {
    client
        .publish(&topics.availability, QoS::AtLeastOnce, true, value)
        .await?;
    Ok(())
}

/// Publishes the current reading, dropping it rather than waiting if MQTT is backed up.
///
/// Deliberately `try_publish`, not `publish`. `publish().await` parks on the client's bounded
/// outgoing channel, and while the broker is unreachable the event loop is not draining it -- so an
/// awaiting publish stalls the caller. That caller is the sampler, which also runs the hooks, so a
/// dead broker would delay shutting services down on a flat battery. State is periodic and
/// retained, so dropping one update is free: the next tick supersedes it.
pub fn publish_state(client: &AsyncClient, topics: &Topics, reading: &Reading) -> Result<()> {
    let payload = serde_json::to_vec(&StatePayload::from(reading))?;
    client.try_publish(&topics.state, QoS::AtLeastOnce, true, payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> Config {
        toml::from_str("[mqtt]\nhost = \"broker\"\ndevice_id = \"pi\"\n").unwrap()
    }

    fn reading() -> Reading {
        Reading {
            bus_voltage_v: 11.4567,
            current_a: -0.6512,
            power_w: 7.4123,
            compensated_voltage_v: 11.5867,
            battery_pct: 68.2941,
            charging: false,
            external_power: false,
        }
    }

    #[test]
    fn our_own_topics_use_the_bare_device_id() {
        // Already namespaced by base_topic, so prefixing here would just stutter.
        let t = Topics::new(&config());
        assert_eq!(t.state, "waveshare-ups/pi/state");
        assert_eq!(t.availability, "waveshare-ups/pi/availability");
    }

    #[test]
    fn discovery_topic_is_prefixed_to_avoid_colliding_with_other_publishers() {
        // Another integration on the same host publishing under node id `pi` with object `battery`
        // would otherwise write to the exact same topic as us.
        let t = Topics::new(&config());
        assert_eq!(
            t.discovery("sensor", "battery"),
            "homeassistant/sensor/waveshare-ups-pi/battery/config"
        );
    }

    #[test]
    fn unique_ids_are_prefixed() {
        let t = Topics::new(&config());
        let p = discovery_payload(&ENTITIES[3], &t); // battery
        assert_eq!(p["unique_id"], "waveshare-ups-pi_battery");
    }

    #[test]
    fn every_ha_facing_key_carries_the_prefix() {
        // The whole point of the change: nothing that shares a global keyspace may be a bare
        // hostname. Guards against a new entity being added without the prefix.
        let t = Topics::new(&config());
        for e in ENTITIES {
            let p = discovery_payload(e, &t);
            let uid = p["unique_id"].as_str().unwrap();
            assert!(
                uid.starts_with("waveshare-ups-"),
                "{} unique_id `{uid}` is not prefixed",
                e.object
            );
            assert!(
                t.discovery(e.component, e.object)
                    .contains("/waveshare-ups-pi/"),
                "{} discovery topic is not prefixed",
                e.object
            );
        }
    }

    #[test]
    fn node_id_prefixes_an_explicitly_configured_device_id_too() {
        // The prefix is about namespacing this integration, not about the hostname specifically, so
        // it applies whether device_id is derived or configured.
        let c: Config = toml::from_str("[mqtt]\nhost = \"b\"\ndevice_id = \"shed\"\n").unwrap();
        let t = Topics::new(&c);
        assert_eq!(t.node_id, "waveshare-ups-shed");
        assert_eq!(t.device_id, "shed");
    }

    #[test]
    fn every_entity_points_at_the_availability_topic() {
        // This is what makes one last will control all six entities. If an entity ever omits it,
        // that entity would stay "live" in HA after the daemon dies.
        let t = Topics::new(&config());
        for e in ENTITIES {
            let p = discovery_payload(e, &t);
            assert_eq!(
                p["availability_topic"], "waveshare-ups/pi/availability",
                "{} missing availability topic",
                e.object
            );
            assert_eq!(p["payload_available"], ONLINE);
            assert_eq!(p["payload_not_available"], OFFLINE);
        }
    }

    #[test]
    fn entities_have_unique_ids_and_share_one_device() {
        let t = Topics::new(&config());
        let mut ids = std::collections::HashSet::new();
        for e in ENTITIES {
            let p = discovery_payload(e, &t);
            assert!(
                ids.insert(p["unique_id"].as_str().unwrap().to_string()),
                "duplicate unique_id for {}",
                e.object
            );
            assert_eq!(p["device"]["identifiers"][0], "waveshare_ups_pi");
        }
        assert_eq!(ids.len(), 6);
    }

    #[test]
    fn required_values_are_all_published() {
        let objects: Vec<_> = ENTITIES.iter().map(|e| e.object).collect();
        for required in ["bus_voltage", "current", "power", "battery"] {
            assert!(objects.contains(&required), "{required} not published");
        }
    }

    #[test]
    fn binary_sensors_omit_state_class_but_carry_payload_on_off() {
        // state_class is invalid on a binary_sensor; HA rejects the entity outright.
        let t = Topics::new(&config());
        for e in ENTITIES.iter().filter(|e| e.component == "binary_sensor") {
            let p = discovery_payload(e, &t);
            assert!(p.get("state_class").is_none(), "{} has state_class", e.object);
            assert!(p.get("unit_of_measurement").is_none());
            assert_eq!(p["payload_on"], "ON");
            assert_eq!(p["payload_off"], "OFF");
        }
    }

    #[test]
    fn numeric_sensors_carry_unit_and_state_class() {
        let t = Topics::new(&config());
        for e in ENTITIES.iter().filter(|e| e.component == "sensor") {
            let p = discovery_payload(e, &t);
            assert_eq!(p["state_class"], "measurement", "{}", e.object);
            assert!(p["unit_of_measurement"].is_string(), "{}", e.object);
        }
    }

    #[test]
    fn value_templates_resolve_against_the_state_payload() {
        // Guards the templates against a field rename in StatePayload: every `value_json.x` must
        // exist in the JSON we actually publish.
        let state = serde_json::to_value(StatePayload::from(&reading())).unwrap();
        for e in ENTITIES {
            let field = e
                .value_template
                .split("value_json.")
                .nth(1)
                .unwrap()
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .next()
                .unwrap();
            assert!(
                state.get(field).is_some(),
                "{} references value_json.{field}, absent from the state payload",
                e.object
            );
        }
    }

    #[test]
    fn state_payload_rounds_without_distorting_values() {
        let p = StatePayload::from(&reading());
        assert_eq!(p.bus_voltage, 11.457);
        assert_eq!(p.current, -0.651);
        assert_eq!(p.battery, 68.3);
        assert!(!p.external_power);
    }

    fn is_discovery_safe(s: &str) -> bool {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
    }

    /// HA requires [a-zA-Z0-9_-] for both the node_id and object_id levels of
    /// `<discovery_prefix>/<component>/[<node_id>/]<object_id>/config`, and rejects the config
    /// silently otherwise. Covers every level of the topic we construct.
    #[test]
    fn every_discovery_topic_level_conforms_to_the_required_character_class() {
        let t = Topics::new(&config());
        assert!(is_discovery_safe(&t.node_id), "node_id {:?}", t.node_id);

        for e in ENTITIES {
            assert!(is_discovery_safe(e.object), "object_id {:?}", e.object);
            assert!(is_discovery_safe(e.component), "component {:?}", e.component);
        }
    }

    #[test]
    fn a_dotted_hostname_produces_a_valid_node_id() {
        // The bug this guards: `raspberrypi.local` used to reach the topic verbatim, and HA
        // dropped the entity without complaint.
        let c: Config = toml::from_str("[mqtt]\nhost = \"b\"\ndevice_id = \"raspberrypi.local\"\n")
            .unwrap();
        let t = Topics::new(&c);

        assert_eq!(t.node_id, "waveshare-ups-raspberrypi-local");
        assert_eq!(
            t.discovery("sensor", "battery"),
            "homeassistant/sensor/waveshare-ups-raspberrypi-local/battery/config"
        );
        assert!(is_discovery_safe(&t.node_id));
    }

    #[test]
    fn unique_ids_match_the_slugified_node_id() {
        let c: Config = toml::from_str("[mqtt]\nhost = \"b\"\ndevice_id = \"raspberrypi.local\"\n")
            .unwrap();
        let t = Topics::new(&c);
        let p = discovery_payload(&ENTITIES[3], &t); // battery
        assert_eq!(p["unique_id"], "waveshare-ups-raspberrypi-local_battery");
    }

    #[test]
    fn device_id_falls_back_to_hostname_and_stays_discovery_safe() {
        let c: Config = toml::from_str("[mqtt]\nhost = \"b\"\n").unwrap();
        assert!(is_discovery_safe(&c.device_id()));
        assert!(is_discovery_safe(&Topics::new(&c).node_id));
    }
}
