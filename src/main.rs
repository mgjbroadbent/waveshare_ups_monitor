//! Waveshare UPS Module 3S monitor.
//!
//! Reads the INA219 over I2C, publishes to MQTT with Home Assistant autodiscovery, and runs
//! external scripts when the battery crosses thresholds or mains power comes and goes.

mod battery;
mod config;
mod hooks;
mod mqtt;
mod sensor;

use anyhow::Result;
use clap::Parser;
use config::Config;
use rumqttc::{Event as MqttEvent, Packet};
use std::path::PathBuf;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(version, about = "Waveshare UPS monitor: MQTT reporting and battery threshold hooks")]
struct Cli {
    #[arg(short, long, default_value = "/etc/waveshare-ups/config.toml")]
    config: PathBuf,

    /// Use a simulated battery ramp instead of real I2C hardware. Exercises MQTT, discovery and the
    /// hook chain on a machine with no UPS attached.
    #[arg(long)]
    simulate: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        // journald adds its own timestamps.
        .without_time()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;
    let topics = mqtt::Topics::new(&config);

    let mut sensor = build_sensor(&config, cli.simulate)?;

    info!(
        device_id = %topics.device_id,
        broker = %format!("{}:{}", config.mqtt.host, config.mqtt.port),
        simulate = cli.simulate,
        "starting"
    );

    let (client, mut eventloop) = mqtt::build_client(&config, &topics);
    let (hook_tx, hook_rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();

    // Hooks run serially in their own task so one slow script cannot stall sampling or MQTT.
    let hook_task = tokio::spawn(hooks::run_hooks(config.hooks.clone(), hook_rx));

    // rumqttc reconnects on its own as long as we keep polling, so errors here are logged, not
    // fatal: a broker outage must not stop us reading the battery or running hooks.
    let mqtt_task = tokio::spawn({
        let client = client.clone();
        let topics = topics.clone();
        let shutdown = shutdown.clone();
        async move {
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    event = eventloop.poll() => match event {
                        Ok(MqttEvent::Incoming(Packet::ConnAck(_))) => {
                            info!("connected to broker");
                            // Republish on every (re)connect: a broker that lost its retained
                            // messages would otherwise leave HA with no entities.
                            //
                            // Spawned rather than awaited inline: awaiting a publish here would
                            // park on the outgoing channel while this very loop is the thing that
                            // drains it. It fits today (7 messages, 32-deep channel), but adding a
                            // few entities would deadlock the connection. Spawning keeps poll()
                            // running regardless.
                            tokio::spawn({
                                let client = client.clone();
                                let topics = topics.clone();
                                async move {
                                    if let Err(e) = mqtt::publish_discovery(&client, &topics).await {
                                        error!("publishing discovery: {e:#}");
                                    }
                                    if let Err(e) =
                                        mqtt::publish_availability(&client, &topics, mqtt::ONLINE)
                                            .await
                                    {
                                        error!("publishing availability: {e:#}");
                                    }
                                }
                            });
                        }
                        Ok(_) => {}
                        Err(e) => {
                            warn!("mqtt connection error, retrying: {e}");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    },
                }
            }
        }
    });

    let sampler = tokio::spawn({
        let client = client.clone();
        let topics = topics.clone();
        let shutdown = shutdown.clone();
        let config = config.clone();
        async move {
            let mut monitor = battery::Monitor::new(&config);
            let mut machine = hooks::HookMachine::new(&config.hooks, config.monitor.confirm_cycles);
            let mut ticker =
                tokio::time::interval(Duration::from_secs(config.monitor.poll_interval_secs));

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => break,
                    _ = ticker.tick() => {}
                }

                let sample = match sensor.read().await {
                    Ok(s) => s,
                    Err(e) => {
                        // Often transient (conversion not ready, bus contention). Log and wait for
                        // the next tick rather than dying.
                        warn!("reading sensor: {e:#}");
                        continue;
                    }
                };

                let reading = monitor.evaluate(sample);
                debug!(
                    bus_v = format!("{:.3}", reading.bus_voltage_v),
                    oc_v = format!("{:.3}", reading.compensated_voltage_v),
                    current_a = format!("{:.3}", reading.current_a),
                    power_w = format!("{:.3}", reading.power_w),
                    battery_pct = format!("{:.1}", reading.battery_pct),
                    charging = reading.charging,
                    external_power = reading.external_power,
                    "reading"
                );

                // Hooks first, and independent of MQTT: losing the broker must not stop us shutting
                // services down on a flat battery.
                for event in machine.update(&reading) {
                    info!(event = event.as_str(), "hook event");
                    if let Err(e) = hook_tx.try_send((event, reading)) {
                        error!(event = event.as_str(), "dropping hook event: {e}");
                    }
                }

                // Non-blocking, and only debug: while the broker is down this would otherwise warn
                // on every tick for as long as the outage lasts. The event loop already reports the
                // connection failure itself.
                if let Err(e) = mqtt::publish_state(&client, &topics, &reading) {
                    debug!("dropping state publish: {e}");
                }
            }
        }
    });

    wait_for_signal().await;
    info!("shutting down");

    // A clean MQTT disconnect suppresses the last will, so an orderly `systemctl stop` would leave
    // HA showing stale values as though we were still alive. Say `offline` ourselves first.
    //
    // Bounded: if the broker is already unreachable, this publish would park on the outgoing
    // channel forever and hang shutdown until systemd's TimeoutStopSec fired. A broker we cannot
    // reach will deliver the last will anyway, so giving up here costs nothing.
    match tokio::time::timeout(
        Duration::from_secs(2),
        mqtt::publish_availability(&client, &topics, mqtt::OFFLINE),
    )
    .await
    {
        Ok(Ok(())) => {
            // Let the event loop put it on the wire before we tear the connection down.
            tokio::time::sleep(Duration::from_millis(250)).await;
            let _ = tokio::time::timeout(Duration::from_secs(2), client.disconnect()).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        Ok(Err(e)) => warn!("publishing offline: {e:#}"),
        Err(_) => warn!("broker unreachable at shutdown; relying on the last will"),
    }

    shutdown.cancel();
    let _ = sampler.await;
    let _ = mqtt_task.await;
    hook_task.abort();
    let _ = hook_task.await;

    info!("stopped");
    Ok(())
}

fn build_sensor(config: &Config, simulate: bool) -> Result<Box<dyn sensor::UpsSensor>> {
    if simulate {
        warn!("running with a simulated sensor: readings are synthetic");
        return Ok(Box::new(sensor::SimulatedSensor::new(
            config.empty_volts(),
            config.full_volts(),
        )));
    }

    #[cfg(target_os = "linux")]
    {
        use anyhow::Context;
        let s = sensor::LinuxSensor::new(config.i2c.bus, config.i2c.address)
            .context("initialising INA219 (is I2C enabled, and is the address right?)")?;
        Ok(Box::new(s))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        anyhow::bail!("I2C is only supported on Linux; use --simulate on this platform")
    }
}

async fn wait_for_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(e) => {
                error!("installing SIGTERM handler: {e}");
                return;
            }
        };
        tokio::select! {
            _ = term.recv() => info!("received SIGTERM"),
            _ = tokio::signal::ctrl_c() => info!("received SIGINT"),
        }
    }

    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
