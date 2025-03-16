use anyhow::Result;
use chrono::Local;
use crossterm::{
    execute,
    terminal::{Clear, ClearType},
};
use serde::Serialize;
use std::io::{Write, stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time;

/// Shared state for our application (HR, power, cadence, etc.)
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
            target_hr: 140, // default target HR (can be adjusted by the user)
            target_power: 0,
            start_time: Instant::now(),
        }
    }
}

/// Record for CSV logging
#[derive(Serialize)]
struct LogRecord {
    timestamp: String,
    elapsed_sec: u64,
    heart_rate: u32,
    power: u32,
    cadence: u32,
    target_hr: u32,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize shared state in a thread-safe container
    let shared_state = Arc::new(Mutex::new(SharedState::new()));

    // Spawn BLE simulation tasks (replace these with actual btleplug logic)
    let hr_state = shared_state.clone();
    tokio::spawn(async move {
        simulate_hr_monitor(hr_state).await;
    });

    let trainer_state = shared_state.clone();
    tokio::spawn(async move {
        simulate_trainer_data(trainer_state).await;
    });

    // Spawn the control loop that adjusts target power based on HR error
    let control_state = shared_state.clone();
    tokio::spawn(async move {
        control_loop(control_state).await;
    });

    // Spawn CSV logging task (logs workout data every second)
    let log_state = shared_state.clone();
    tokio::spawn(async move {
        if let Err(e) = csv_logging_task(log_state).await {
            eprintln!("CSV logging error: {:?}", e);
        }
    });

    // Run the terminal UI on the main task
    terminal_ui(shared_state).await;

    Ok(())
}

/// Simulated BLE heart rate monitor task:
/// In a real application, use `btleplug` to connect and subscribe to the Heart Rate Measurement characteristic.
async fn simulate_hr_monitor(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_secs(1));
    let mut hr_value = 130; // starting HR value
    loop {
        interval.tick().await;
        // Simulate a gradual increase then reset of the heart rate
        hr_value = if hr_value < 160 { hr_value + 1 } else { 130 };
        let mut data = state.lock().unwrap();
        data.current_hr = hr_value;
    }
}

/// Simulated BLE trainer data task:
/// In a real application, use `btleplug` to connect and subscribe to FTMS characteristics.
async fn simulate_trainer_data(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_secs(1));
    let mut cadence_value = 80; // starting cadence value
    loop {
        interval.tick().await;
        // Simulate cadence changes
        cadence_value = if cadence_value < 95 {
            cadence_value + 1
        } else {
            80
        };
        let mut data = state.lock().unwrap();
        // Simulate that current power moves gradually toward the target power
        if data.current_power < data.target_power {
            data.current_power += 5;
        } else if data.current_power > data.target_power {
            data.current_power -= 5;
        }
        data.cadence = cadence_value;
    }
}

/// Control loop that adjusts the target power based on the heart rate error
async fn control_loop(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_secs(1));
    // Proportional gain parameter: a user-adjustable setting for smoother response
    let k: f32 = 1.0;
    loop {
        interval.tick().await;
        let mut data = state.lock().unwrap();
        let error = data.target_hr as i32 - data.current_hr as i32;
        // Calculate power adjustment using a proportional controller
        let adjustment = (k * error as f32).round() as i32;
        let new_power = data.target_power as i32 + adjustment;
        // Clamp the target power between 0 and 400 watts (typical limits)
        data.target_power = new_power.clamp(0, 400) as u32;

        // In a real application, write the new power target to the trainer via BLE here.
    }
}

/// Terminal UI task: clears the screen and prints current state every 500ms
async fn terminal_ui(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_millis(500));
    loop {
        interval.tick().await;
        // Clear the terminal screen
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

/// CSV Logging task: writes a record to 'workout.csv' every second
async fn csv_logging_task(state: Arc<Mutex<SharedState>>) -> Result<()> {
    use std::fs::OpenOptions;
    use std::io::Write;

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
