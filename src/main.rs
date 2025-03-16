use anyhow::Result;
use btleplug::api::{Central, Manager as _, Peripheral as _};
use btleplug::platform::Manager;
use chrono::Local;
use crossterm::{
    execute,
    terminal::{Clear, ClearType},
};
use futures::stream::StreamExt;
use serde::{Deserialize, Serialize};
use std::io::stdout;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time;

// =====================
// CONFIGURATION
// =====================

#[derive(Debug, Deserialize)]
struct Config {
    heart_rate_device: Option<String>,
    trainer_device: Option<String>,
}

fn load_config() -> Result<Config> {
    let contents = std::fs::read_to_string("config.toml")?;
    let config: Config = toml::from_str(&contents)?;
    Ok(config)
}

// =====================
// SHARED STATE & CSV LOGGING
// =====================

#[derive(Debug)]
struct SharedState {
    current_hr: u32,
    current_power: u32,
    cadence: u32,
    target_hr: u32,
    target_power: u32,
    start_time: Instant,
}

impl SharedState {
    fn new() -> Self {
        Self {
            current_hr: 0,
            current_power: 0,
            cadence: 0,
            target_hr: 140, // Default target HR; can be changed via UI or config later.
            target_power: 0,
            start_time: Instant::now(),
        }
    }
}

#[derive(Serialize)]
struct LogRecord {
    timestamp: String,
    elapsed_sec: u64,
    heart_rate: u32,
    power: u32,
    cadence: u32,
    target_hr: u32,
}

// =====================
// BLE DEVICE CONNECTION & NOTIFICATIONS
// =====================

/// Scans for BLE devices and returns the HR device and Trainer device found based on config.
async fn connect_devices(
    config: &Config,
) -> Result<(
    btleplug::platform::Peripheral,
    btleplug::platform::Peripheral,
)> {
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or(anyhow::anyhow!("No BLE adapters found"))?;
    adapter.start_scan(Default::default()).await?;
    println!("Scanning for devices for 10 seconds...");
    time::sleep(Duration::from_secs(10)).await;

    let peripherals = adapter.peripherals().await?;
    let mut hr_device: Option<btleplug::platform::Peripheral> = None;
    let mut trainer_device: Option<btleplug::platform::Peripheral> = None;

    for peripheral in peripherals {
        if let Some(properties) = peripheral.properties().await? {
            if let Some(name) = properties.local_name {
                if let Some(ref hr_filter) = config.heart_rate_device {
                    if name.to_lowercase().contains(&hr_filter.to_lowercase()) {
                        println!("Found HR device: {}", name);
                        hr_device = Some(peripheral.clone());
                    }
                }
                if let Some(ref trainer_filter) = config.trainer_device {
                    if name.to_lowercase().contains(&trainer_filter.to_lowercase()) {
                        println!("Found Trainer device: {}", name);
                        trainer_device = Some(peripheral.clone());
                    }
                }
            }
        }
    }
    let hr_device = hr_device.ok_or(anyhow::anyhow!("Heart rate device not found"))?;
    let trainer_device = trainer_device.ok_or(anyhow::anyhow!("Trainer device not found"))?;

    if !hr_device.is_connected().await? {
        hr_device.connect().await?;
    }
    if !trainer_device.is_connected().await? {
        trainer_device.connect().await?;
    }

    // Discover services for both devices.
    hr_device.discover_services().await?;
    trainer_device.discover_services().await?;

    Ok((hr_device, trainer_device))
}

/// Subscribe to Heart Rate notifications (characteristic UUID 0x2A37).
async fn run_hr_notifications(
    hr_device: btleplug::platform::Peripheral,
    state: Arc<Mutex<SharedState>>,
) -> Result<()> {
    // Look for the Heart Rate Measurement characteristic.
    use uuid::Uuid;

    let hr_uuid = Uuid::parse_str("00002A37-0000-1000-8000-00805F9B34FB")?;
    let hr_char = hr_device
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == hr_uuid)
        .ok_or(anyhow::anyhow!("HR characteristic not found"))?;

    hr_device.subscribe(&hr_char).await?;
    println!("Subscribed to HR notifications.");

    let mut notif_stream = hr_device.notifications().await?;
    while let Some(data) = notif_stream.next().await {
        // According to the spec, the first byte is flags, next is the HR value.
        if data.value.len() >= 2 {
            let flags = data.value[0];
            let hr = if flags & 0x01 == 0 {
                // 8-bit heart rate
                data.value[1] as u32
            } else if data.value.len() >= 3 {
                // 16-bit heart rate
                u16::from_le_bytes([data.value[1], data.value[2]]) as u32
            } else {
                0
            };
            let mut s = state.lock().unwrap();
            s.current_hr = hr;
        }
    }
    Ok(())
}

/// Subscribe to Trainer notifications for Indoor Bike Data (characteristic UUID 0x2A5B).
async fn run_trainer_notifications(
    trainer_device: btleplug::platform::Peripheral,
    state: Arc<Mutex<SharedState>>,
) -> Result<()> {
    // Look for the Indoor Bike Data characteristic.
    use uuid::Uuid;

    let bike_uuid = Uuid::parse_str("00002A5B-0000-1000-8000-00805F9B34FB")?;
    let bike_data_char = trainer_device
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == bike_uuid)
        .ok_or(anyhow::anyhow!("Indoor Bike Data characteristic not found"))?;

    trainer_device.subscribe(&bike_data_char).await?;
    println!("Subscribed to Trainer notifications.");

    let mut notif_stream = trainer_device.notifications().await?;
    while let Some(data) = notif_stream.next().await {
        // This is a simplified parser. The FTMS Indoor Bike Data characteristic can be complex.
        // Here, we assume the first two bytes are instantaneous power (u16 little-endian)
        // and the third byte is cadence.
        if data.value.len() >= 3 {
            let power = u16::from_le_bytes([data.value[0], data.value[1]]) as u32;
            let cadence = data.value[2] as u32;
            let mut s = state.lock().unwrap();
            s.current_power = power;
            s.cadence = cadence;
        }
    }
    Ok(())
}

// =====================
// CONTROL LOOP, CSV LOGGING & TERMINAL UI
// =====================

/// Control loop: adjusts target power based on heart rate error using a proportional controller.
async fn control_loop(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_secs(1));
    let k: f32 = 1.0; // Proportional gain (user-adjustable for smoother response)
    loop {
        interval.tick().await;
        let mut data = state.lock().unwrap();
        let error = data.target_hr as i32 - data.current_hr as i32;
        let adjustment = (k * error as f32).round() as i32;
        let new_power = data.target_power as i32 + adjustment;
        data.target_power = new_power.clamp(0, 400) as u32;
        // In a full implementation, write the new target power to the trainer via BLE here.
    }
}

/// CSV logging task: writes a record to 'workout.csv' every second.
async fn csv_logging_task(state: Arc<Mutex<SharedState>>) -> Result<()> {
    use std::fs::OpenOptions;

    let file_path = "workout.csv";
    let mut wtr = csv::Writer::from_writer(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(file_path)?,
    );
    let mut interval = time::interval(Duration::from_secs(1));
    loop {
        interval.tick().await;
        let data = state.lock().unwrap();
        let elapsed_sec = data.start_time.elapsed().as_secs();
        let record = LogRecord {
            timestamp: Local::now().to_rfc3339(),
            elapsed_sec,
            heart_rate: data.current_hr,
            power: data.current_power,
            cadence: data.cadence,
            target_hr: data.target_hr,
        };
        wtr.serialize(record)?;
        wtr.flush()?;
    }
}

/// Terminal UI: clears and redraws the screen every 500ms with the latest state.
async fn terminal_ui(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;
        execute!(stdout(), Clear(ClearType::All)).unwrap();
        let data = state.lock().unwrap();
        let elapsed = data.start_time.elapsed().as_secs();
        println!("=== HearTrace ===");
        println!("Elapsed Time: {} sec", elapsed);
        println!("Current HR: {} bpm", data.current_hr);
        println!("Current Power: {} W", data.current_power);
        println!("Cadence: {} rpm", data.cadence);
        println!("Target HR: {} bpm", data.target_hr);
        println!("Target Power: {} W", data.target_power);
        println!("\nPress Ctrl+C to exit.");
    }
}

// =====================
// MAIN FUNCTION
// =====================

#[tokio::main]
async fn main() -> Result<()> {
    // Load configuration from config.toml.
    let config = load_config()?;
    println!("Loaded config: {:?}", config);

    // Initialize shared state.
    let shared_state = Arc::new(Mutex::new(SharedState::new()));

    // Scan for devices and connect.
    let (hr_device, trainer_device) = connect_devices(&config).await?;

    // Spawn tasks for BLE notifications.
    let hr_state = shared_state.clone();
    let hr_task = tokio::spawn(async move {
        if let Err(e) = run_hr_notifications(hr_device, hr_state).await {
            eprintln!("HR notifications error: {:?}", e);
        }
    });

    let trainer_state = shared_state.clone();
    let trainer_task = tokio::spawn(async move {
        if let Err(e) = run_trainer_notifications(trainer_device, trainer_state).await {
            eprintln!("Trainer notifications error: {:?}", e);
        }
    });

    // Spawn control loop and CSV logging tasks.
    let control_state = shared_state.clone();
    tokio::spawn(async move {
        control_loop(control_state).await;
    });

    let log_state = shared_state.clone();
    tokio::spawn(async move {
        if let Err(e) = csv_logging_task(log_state).await {
            eprintln!("CSV logging error: {:?}", e);
        }
    });

    // Run the terminal UI on the main task.
    terminal_ui(shared_state).await;

    // Await the BLE tasks (though terminal_ui is blocking until exit).
    let _ = tokio::join!(hr_task, trainer_task);

    Ok(())
}
