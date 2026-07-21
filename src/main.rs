use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::delay::FreeRtos;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::{EspMqttClient, MqttClientConfiguration, QoS};
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi};

// --- Config loaded from cfg.toml at build time (see cfg.toml.example) ---
#[toml_cfg::toml_config]
pub struct Config {
    #[default("")]
    wifi_ssid: &'static str,
    #[default("")]
    wifi_pass: &'static str,
    #[default("mqtt://broker.example.com:1883")]
    mqtt_url: &'static str,
    #[default("esp32-blinker")]
    mqtt_client_id: &'static str,
    #[default("esp32/blink/speed_level")]
    topic_speed: &'static str,
    #[default("esp32/blink/status")]
    topic_status: &'static str,
}

// --- Firmware behavior constants (not deployment config — these feed the
// compile-time DELAY_TABLE below, so they must stay real `const`s) ---
const MIN_DELAY_MS: u32 = 50; // delay at level 10 (fastest)
const MAX_DELAY_MS: u32 = 1000; // delay at level 1 (slowest)
const MAX_LEVEL: u32 = 10;
const HEARTBEAT_SECS: u64 = 30;
const OFF_POLL_MS: u32 = 100; // how often to re-check level while off (level 0)

/// Index 0 = level 0 (off, None). Index 1..=10 = levels 1..=10 (Some(delay_ms)).
/// Computed entirely at compile time — lives in flash/.rodata, zero runtime init cost.
const DELAY_TABLE: [Option<u32>; (MAX_LEVEL + 1) as usize] = build_delay_table();

const fn build_delay_table() -> [Option<u32>; (MAX_LEVEL + 1) as usize] {
    let mut table = [None; (MAX_LEVEL + 1) as usize];
    let span = MAX_DELAY_MS - MIN_DELAY_MS;
    let steps = MAX_LEVEL - 1; // 9 steps between level 1 and level 10

    // const fn can't use `for` on stable, so `while` does the iteration
    let mut level = 1;
    while level <= MAX_LEVEL {
        let delay = MAX_DELAY_MS - (span * (level - 1)) / steps;
        table[level as usize] = Some(delay);
        level += 1;
    }
    table
}

/// Looks up the blink half-period in ms for a speed level (0..=10).
/// 0 => None (LED off). Values above MAX_LEVEL are clamped.
fn level_to_delay_ms(level: u32) -> Option<u32> {
    let idx = level.min(MAX_LEVEL) as usize;
    DELAY_TABLE[idx]
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let app_config = CONFIG;

    if app_config.wifi_ssid.is_empty() {
        anyhow::bail!(
            "wifi_ssid is empty — fill in cfg.toml (see cfg.toml.example) before building"
        );
    }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // --- WiFi setup ---
    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
    )?;

    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: app_config.wifi_ssid.try_into().unwrap(),
        password: app_config.wifi_pass.try_into().unwrap(),
        auth_method: AuthMethod::WPA2Personal,
        ..Default::default()
    }))?;

    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;
    log::info!("WiFi connected");

    // --- Shared state: current speed level, 0..=10 ---
    let level = Arc::new(AtomicU32::new(5)); // sane mid-speed default
    let level_mqtt = level.clone();

    // --- MQTT setup ---
    let mqtt_config = MqttClientConfiguration {
        client_id: Some(app_config.mqtt_client_id),
        ..Default::default()
    };

    let (client, mut connection) = EspMqttClient::new(app_config.mqtt_url, &mqtt_config)?;
    // Shared so both the subscriber thread and the status/heartbeat publishers can use it.
    let client = Arc::new(Mutex::new(client));
    let client_for_sub = client.clone();
    let client_for_status = client.clone();
    let client_for_heartbeat = client.clone();

    // --- Status-publish channel: publish_status() calls the blocking `client.publish()`,
    // which deadlocks if called from the connection-draining thread below (that thread must
    // stay free to keep calling `connection.next()`, which is also what drives the client's
    // internal state machine forward). So the draining thread only ever sends over this
    // channel; a separate thread owns the client and does the actual publish. ---
    let (status_tx, status_rx) = mpsc::channel::<(u32, &'static str)>();

    // --- Connection-draining thread: must be pumping `connection.next()` continuously or
    // the client's internal connect/subscribe state machine never progresses (this is required
    // by esp-idf-svc's design, see esp-rs/esp-idf-svc#441) ---
    {
        thread::spawn(move || {
            while let Ok(event) = connection.next() {
                if let esp_idf_svc::mqtt::client::EventPayload::Received { data, .. } =
                    event.payload()
                {
                    if let Ok(text) = std::str::from_utf8(data) {
                        match text.trim().parse::<u32>() {
                            Ok(lvl) if lvl <= MAX_LEVEL => {
                                log::info!("New speed level: {lvl}");
                                level_mqtt.store(lvl, Ordering::Relaxed);
                                let _ = status_tx.send((lvl, "updated"));
                            }
                            Ok(lvl) => {
                                log::warn!("Rejected out-of-range level: {lvl}");
                                let _ = status_tx.send((lvl, "rejected_out_of_range"));
                            }
                            Err(_) => {
                                log::warn!("Ignoring non-numeric payload: {text}");
                            }
                        }
                    }
                }
            }
            log::warn!("MQTT connection closed");
        });
    }

    // --- Status-publisher thread: the only place that actually calls publish() for status
    // updates triggered by incoming commands (see channel comment above). ---
    {
        let topic_status = app_config.topic_status;
        thread::spawn(move || {
            while let Ok((lvl, reason)) = status_rx.recv() {
                publish_status(&client_for_status, topic_status, lvl, reason);
            }
        });
    }

    // --- Subscriber thread: retries subscribe() until it succeeds. Must run concurrently
    // with (not gated on events from) the draining thread above, and on a *different* thread
    // than the one calling `connection.next()`, or the retries can't make progress. ---
    {
        let topic_speed = app_config.topic_speed;
        thread::spawn(move || loop {
            match client_for_sub
                .lock()
                .unwrap()
                .subscribe(topic_speed, QoS::AtLeastOnce)
            {
                Ok(_) => {
                    log::info!("Subscribed to {topic_speed}");
                    break;
                }
                Err(e) => {
                    log::warn!("Failed to subscribe to {topic_speed}: {e:?}, retrying...");
                    thread::sleep(Duration::from_millis(500));
                }
            }
        });
    }

    // --- Heartbeat thread: periodically republishes current state ---
    {
        let level = level.clone();
        let topic_status = app_config.topic_status;
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
            let lvl = level.load(Ordering::Relaxed);
            publish_status(&client_for_heartbeat, topic_status, lvl, "heartbeat");
        });
    }

    // --- Blink loop ---
    let mut led = PinDriver::output(peripherals.pins.gpio2)?;

    loop {
        let lvl = level.load(Ordering::Relaxed);

        match level_to_delay_ms(lvl) {
            None => {
                // Level 0: stay off, poll periodically so a new command is picked up quickly
                led.set_low()?;
                FreeRtos::delay_ms(OFF_POLL_MS);
            }
            Some(delay) => {
                led.set_high()?;
                FreeRtos::delay_ms(delay);
                led.set_low()?;
                FreeRtos::delay_ms(delay);
            }
        }
    }
}

/// Publishes a small JSON status payload, e.g. {"level":5,"delay_ms":578,"reason":"updated"}
fn publish_status(
    client: &Arc<Mutex<EspMqttClient<'_>>>,
    topic_status: &str,
    level: u32,
    reason: &str,
) {
    let delay_ms = level_to_delay_ms(level);
    let payload = match delay_ms {
        Some(ms) => format!(r#"{{"level":{level},"delay_ms":{ms},"reason":"{reason}"}}"#),
        None => format!(r#"{{"level":{level},"delay_ms":null,"reason":"{reason}"}}"#),
    };
    if let Err(e) =
        client
            .lock()
            .unwrap()
            .publish(topic_status, QoS::AtLeastOnce, false, payload.as_bytes())
    {
        log::warn!("Failed to publish status: {e:?}");
    }
}
