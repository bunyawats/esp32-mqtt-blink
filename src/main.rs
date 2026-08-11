use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
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
    #[default("esp32/speed_level")]
    topic_speed: &'static str,
    #[default("esp32/switch")]
    topic_switch: &'static str,
    #[default("esp32/status")]
    topic_status: &'static str,
}

// --- Firmware behavior constants (not deployment config — these feed the
// compile-time DELAY_TABLE below, so they must stay real `const`s) ---
const MIN_DELAY_MS: u32 = 50; // delay at level 10 (fastest)
const MAX_DELAY_MS: u32 = 1000; // delay at level 1 (slowest)
const MAX_LEVEL: u32 = 10;
const HEARTBEAT_SECS: u64 = 30;
const OFF_POLL_MS: u32 = 100; // how often to re-check state while static (mode Off/On)
const OFFLINE_FALLBACK_SECS: u64 = 10; // boot-time window to wait for MQTT subscribe before assuming offline

// --- LED display mode: three mutually exclusive top-level states, not a level plus a gate.
// `Off`/`On` are symmetric static states — the main loop just holds the pin and polls for a
// new command, with zero blink-timing logic running. Only `Blink` ever touches
// `level_to_delay_ms`. Stored as a plain u8 in an AtomicU8 since `std::sync::atomic` has no
// generic atomic-enum type. See IMPLEMENTATION_PLAN.md for the full design rationale. ---
const MODE_OFF: u8 = 0;
const MODE_ON: u8 = 1;
const MODE_BLINK: u8 = 2;

fn mode_str(mode: u8) -> &'static str {
    match mode {
        MODE_OFF => "off",
        MODE_ON => "on",
        _ => "blink",
    }
}

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

/// Looks up the blink half-period in ms for a speed level (0..=10), used only in `Blink` mode.
/// 0 => None (degenerate always-off pattern). Values above MAX_LEVEL are clamped.
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

    // --- Shared state ---
    // `level`: current blink speed, 0..=10, only meaningful while `mode == MODE_BLINK`.
    // `mode`: which of the three top-level LED states is active (see MODE_* consts above).
    // `got_real_command`: true once any real `blink`/`switch` command has been processed from
    // the network — guards the boot-time defaults below so they never clobber a real command,
    // regardless of how late they fire.
    // `subscribed`: true once both topics are subscribed — guards the offline-fallback timer so
    // a subscribe that succeeds just under the wire can't be clobbered by a late-firing fallback
    // (got_real_command alone isn't enough for that, since neither boot default sets it).
    let level = Arc::new(AtomicU32::new(5)); // sane mid-speed default, dormant until Blink mode is entered
    let mode = Arc::new(AtomicU8::new(MODE_OFF));
    let got_real_command = Arc::new(AtomicBool::new(false));
    let subscribed = Arc::new(AtomicBool::new(false));

    let level_mqtt = level.clone();
    let mode_mqtt = mode.clone();
    let got_real_command_mqtt = got_real_command.clone();

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
    // channel; a separate thread owns the client and does the actual publish. Cloned per
    // producer thread since `mpsc::Sender` is `Clone` but the channel has one receiver. ---
    let (status_tx, status_rx) = mpsc::channel::<(u32, u8, &'static str)>();
    let status_tx_offline = status_tx.clone();
    let status_tx_connect = status_tx.clone();

    // --- Connection-draining thread: must be pumping `connection.next()` continuously or
    // the client's internal connect/subscribe state machine never progresses (this is required
    // by esp-idf-svc's design, see esp-rs/esp-idf-svc#441) ---
    {
        let topic_speed = app_config.topic_speed;
        let topic_switch = app_config.topic_switch;
        thread::spawn(move || {
            while let Ok(event) = connection.next() {
                if let esp_idf_svc::mqtt::client::EventPayload::Received { topic, data, .. } =
                    event.payload()
                {
                    if topic == Some(topic_speed) {
                        if let Ok(text) = std::str::from_utf8(data) {
                            match text.trim().parse::<u32>() {
                                Ok(lvl) if lvl <= MAX_LEVEL => {
                                    log::info!("New speed level: {lvl}");
                                    level_mqtt.store(lvl, Ordering::Relaxed);
                                    mode_mqtt.store(MODE_BLINK, Ordering::Relaxed);
                                    got_real_command_mqtt.store(true, Ordering::Relaxed);
                                    let _ = status_tx.send((lvl, MODE_BLINK, "updated"));
                                }
                                Ok(lvl) => {
                                    log::warn!("Rejected out-of-range level: {lvl}");
                                    got_real_command_mqtt.store(true, Ordering::Relaxed);
                                    let _ = status_tx.send((
                                        lvl,
                                        mode_mqtt.load(Ordering::Relaxed),
                                        "rejected_out_of_range",
                                    ));
                                }
                                Err(_) => {
                                    log::warn!("Ignoring non-numeric payload: {text}");
                                }
                            }
                        }
                    } else if topic == Some(topic_switch) {
                        if let Ok(text) = std::str::from_utf8(data) {
                            let new_mode = match text.trim() {
                                "on" => Some((MODE_ON, "switch_on")),
                                "off" => Some((MODE_OFF, "switch_off")),
                                "toggle" => {
                                    let cur = mode_mqtt.load(Ordering::Relaxed);
                                    let next = if cur == MODE_OFF { MODE_ON } else { MODE_OFF };
                                    Some((next, "switch_toggle"))
                                }
                                other => {
                                    log::warn!("Ignoring invalid switch payload: {other}");
                                    None
                                }
                            };

                            got_real_command_mqtt.store(true, Ordering::Relaxed);
                            match new_mode {
                                Some((m, reason)) => {
                                    mode_mqtt.store(m, Ordering::Relaxed);
                                    let _ =
                                        status_tx.send((level_mqtt.load(Ordering::Relaxed), m, reason));
                                }
                                None => {
                                    let _ = status_tx.send((
                                        level_mqtt.load(Ordering::Relaxed),
                                        mode_mqtt.load(Ordering::Relaxed),
                                        "rejected_invalid_switch",
                                    ));
                                }
                            }
                        }
                    } else {
                        log::warn!("Received message on unexpected topic: {topic:?}");
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
            while let Ok((lvl, m, reason)) = status_rx.recv() {
                publish_status(&client_for_status, topic_status, lvl, m, reason);
            }
        });
    }

    // --- Subscriber thread: retries subscribe() until it succeeds, for both topics in
    // sequence, then applies the "connected" boot-time default. Must run concurrently with
    // (not gated on events from) the draining thread above, and on a *different* thread than
    // the one calling `connection.next()`, or the retries can't make progress. ---
    {
        let topic_speed = app_config.topic_speed;
        let topic_switch = app_config.topic_switch;
        let level = level.clone();
        let mode = mode.clone();
        let got_real_command = got_real_command.clone();
        let subscribed = subscribed.clone();
        let status_tx = status_tx_connect;
        thread::spawn(move || {
            for topic in [topic_speed, topic_switch] {
                loop {
                    match client_for_sub.lock().unwrap().subscribe(topic, QoS::AtLeastOnce) {
                        Ok(_) => {
                            log::info!("Subscribed to {topic}");
                            break;
                        }
                        Err(e) => {
                            log::warn!("Failed to subscribe to {topic}: {e:?}, retrying...");
                            thread::sleep(Duration::from_millis(500));
                        }
                    }
                }
            }

            subscribed.store(true, Ordering::Relaxed);

            // Boot-time "connected" default: only applies if no real command has arrived yet.
            // Deliberately allowed to fire even after the offline fallback below already did —
            // that's the intended "was offline, now connected" transition, not a bug.
            if !got_real_command.load(Ordering::Relaxed) {
                mode.store(MODE_ON, Ordering::Relaxed);
                let _ = status_tx.send((level.load(Ordering::Relaxed), MODE_ON, "connected_default"));
            }
        });
    }

    // --- Offline-fallback thread: one-time boot-window check, not a continuous re-check on a
    // later mid-session disconnect. Gated on `subscribed`, not `got_real_command` — a fast
    // connect must not be clobbered by this firing late, since a successful subscribe doesn't
    // itself set `got_real_command` (only a real inbound command does). ---
    {
        let level = level.clone();
        let mode = mode.clone();
        let subscribed = subscribed.clone();
        let status_tx = status_tx_offline;
        thread::spawn(move || {
            thread::sleep(Duration::from_secs(OFFLINE_FALLBACK_SECS));
            if !subscribed.load(Ordering::Relaxed) {
                log::warn!("MQTT still not subscribed after {OFFLINE_FALLBACK_SECS}s — applying offline fallback");
                level.store(MAX_LEVEL, Ordering::Relaxed);
                mode.store(MODE_BLINK, Ordering::Relaxed);
                let _ = status_tx.send((MAX_LEVEL, MODE_BLINK, "offline_fallback"));
            }
        });
    }

    // --- Heartbeat thread: periodically republishes current state ---
    {
        let level = level.clone();
        let mode = mode.clone();
        let topic_status = app_config.topic_status;
        thread::spawn(move || loop {
            thread::sleep(Duration::from_secs(HEARTBEAT_SECS));
            let lvl = level.load(Ordering::Relaxed);
            let m = mode.load(Ordering::Relaxed);
            publish_status(&client_for_heartbeat, topic_status, lvl, m, "heartbeat");
        });
    }

    // --- Main display loop: `Off`/`On` are symmetric static branches (no blink timing runs at
    // all); `Blink` is the only branch that ever consults `level_to_delay_ms`. ---
    let mut led = PinDriver::output(peripherals.pins.gpio2)?;

    loop {
        match mode.load(Ordering::Relaxed) {
            MODE_OFF => {
                led.set_low()?;
                FreeRtos::delay_ms(OFF_POLL_MS);
            }
            MODE_ON => {
                led.set_high()?;
                FreeRtos::delay_ms(OFF_POLL_MS);
            }
            _ => {
                let lvl = level.load(Ordering::Relaxed);
                match level_to_delay_ms(lvl) {
                    None => {
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
    }
}

/// Publishes a small JSON status payload, e.g.
/// {"mode":"blink","level":5,"delay_ms":578,"reason":"updated"} or
/// {"mode":"on","level":null,"delay_ms":null,"reason":"switch_on"}.
/// `level`/`delay_ms` are only meaningful in `Blink` mode — both are `null` otherwise, since
/// neither describes anything actually driving the LED while it's in a static Off/On state.
fn publish_status(
    client: &Arc<Mutex<EspMqttClient<'_>>>,
    topic_status: &str,
    level: u32,
    mode: u8,
    reason: &str,
) {
    let mode_s = mode_str(mode);
    let payload = if mode == MODE_BLINK {
        match level_to_delay_ms(level) {
            Some(ms) => {
                format!(r#"{{"mode":"{mode_s}","level":{level},"delay_ms":{ms},"reason":"{reason}"}}"#)
            }
            None => {
                format!(r#"{{"mode":"{mode_s}","level":{level},"delay_ms":null,"reason":"{reason}"}}"#)
            }
        }
    } else {
        format!(r#"{{"mode":"{mode_s}","level":null,"delay_ms":null,"reason":"{reason}"}}"#)
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
