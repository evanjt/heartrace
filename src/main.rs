use anyhow::Result;
use btleplug::api::{Central, Manager as _, Peripheral as _};
use btleplug::platform::{Manager, Peripheral};
use chrono::Local;
use crossterm::{
    event::{self, Event as CEvent, KeyCode, KeyEvent},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use futures::stream::StreamExt;
use serde::Serialize;
use std::cmp::{Ordering, Reverse};
use std::io::{Stdout, Write, stdout};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::time;
use tui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Span, Spans},
    widgets::{Block, Borders, Cell, Row, Table},
};
use uuid::Uuid;

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
            target_hr: 140, // Default target HR.
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
// DEVICE SELECTION TUI STRUCTURES
// =====================

#[derive(PartialEq)]
enum SelectionStage {
    Hr,
    Trainer,
}

struct App {
    // Vector of devices with a stable local id, name, RSSI, and Peripheral.
    devices: Vec<(u32, String, i16, Peripheral)>,
    // Currently highlighted index in the list.
    selected_index: usize,
    // Which device are we selecting: HR or Trainer?
    stage: SelectionStage,
    // Once selected, store the chosen peripherals.
    hr_device: Option<Peripheral>,
    trainer_device: Option<Peripheral>,
    // Next stable id to assign.
    next_id: u32,
}

impl App {
    fn new() -> Self {
        Self {
            devices: Vec::new(),
            selected_index: 0,
            stage: SelectionStage::Hr,
            hr_device: None,
            trainer_device: None,
            next_id: 0,
        }
    }
}

// =====================
// HELPER FUNCTION: RSSI BAR
// =====================

/// Computes a signal strength bar (10-character wide) and chooses a color based on the RSSI.
/// Assumes excellent RSSI = -40, poor RSSI = -100.
fn rssi_bar(rssi: i16) -> (String, Color) {
    let max_rssi = -40;
    let min_rssi = -100;
    let bar_length = 10;
    let ratio = ((rssi - min_rssi) as f32 / (max_rssi - min_rssi) as f32).clamp(0.0, 1.0);
    let fill_count = (ratio * bar_length as f32).round() as usize;
    let filled = "█".repeat(fill_count);
    let empty = " ".repeat(bar_length - fill_count);
    let bar = format!("{}{}", filled, empty);
    let color = if ratio >= 0.66 {
        Color::Green
    } else if ratio >= 0.33 {
        Color::Yellow
    } else {
        Color::Red
    };
    (bar, color)
}

// =====================
// BLE NOTIFICATIONS
// =====================

/// Subscribes to Heart Rate notifications using the standard HR Measurement UUID.
async fn run_hr_notifications(hr_device: Peripheral, state: Arc<Mutex<SharedState>>) -> Result<()> {
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
        if data.value.len() >= 2 {
            let flags = data.value[0];
            let hr = if flags & 0x01 == 0 {
                data.value[1] as u32
            } else if data.value.len() >= 3 {
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

/// Subscribes to Trainer notifications. If the Indoor Bike Data characteristic (UUID 0x2A5B)
/// isn’t found, prints available characteristic UUIDs and logs raw data.
async fn run_trainer_notifications(
    trainer_device: Peripheral,
    state: Arc<Mutex<SharedState>>,
) -> Result<()> {
    let bike_uuid = Uuid::parse_str("00002A5B-0000-1000-8000-00805F9B34FB")?;
    let characteristics = trainer_device.characteristics();
    println!("Trainer device characteristics:");
    for c in &characteristics {
        println!("  UUID: {}", c.uuid);
    }
    let bike_data_char = characteristics
        .into_iter()
        .find(|c| c.uuid == bike_uuid)
        .ok_or(anyhow::anyhow!(
            "Indoor Bike Data characteristic not found. See above for available characteristics."
        ))?;
    trainer_device.subscribe(&bike_data_char).await?;
    println!(
        "Subscribed to Trainer notifications for characteristic {}",
        bike_data_char.uuid
    );
    let mut notif_stream = trainer_device.notifications().await?;
    while let Some(data) = notif_stream.next().await {
        // Print raw data for debugging.
        println!("Raw trainer data: {:?}", data.value);
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
// CONTROL LOOP & CSV LOGGING
// =====================

/// Adjusts target power based on heart rate error using a simple proportional controller.
async fn control_loop(state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_secs(1));
    let k: f32 = 1.0; // Proportional gain
    loop {
        interval.tick().await;
        let mut data = state.lock().unwrap();
        let error = data.target_hr as i32 - data.current_hr as i32;
        let adjustment = (k * error as f32).round() as i32;
        let new_power = data.target_power as i32 + adjustment;
        data.target_power = new_power.clamp(0, 400) as u32;
        // (In production, write the new target power to the trainer via BLE.)
    }
}

/// Logs workout data to a CSV file every second.
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

// =====================
// LIVE DATA TUI (Dashboard)
// =====================

fn draw_live_dashboard<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &SharedState,
) -> Result<()> {
    terminal.draw(|f| {
        let size = f.size();
        let block = Block::default()
            .borders(tui::widgets::Borders::ALL)
            .title("Live Data");
        f.render_widget(block, size);

        let elapsed = state.start_time.elapsed().as_secs();
        let text = vec![
            Spans::from(Span::raw(format!("Elapsed Time: {} sec", elapsed))),
            Spans::from(Span::raw(format!("Current HR: {} bpm", state.current_hr))),
            Spans::from(Span::raw(format!(
                "Current Power: {} W",
                state.current_power
            ))),
            Spans::from(Span::raw(format!("Cadence: {} rpm", state.cadence))),
            Spans::from(Span::raw(format!("Target HR: {} bpm", state.target_hr))),
            Spans::from(Span::raw(format!("Target Power: {} W", state.target_power))),
            Spans::from(Span::raw("Press 'q' to quit.")),
        ];
        let paragraph = tui::widgets::Paragraph::new(text)
            .block(Block::default().borders(tui::widgets::Borders::ALL));
        f.render_widget(paragraph, size);
    })?;
    Ok(())
}

async fn live_data_tui(state: Arc<Mutex<SharedState>>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    loop {
        {
            let data = state.lock().unwrap();
            draw_live_dashboard(&mut terminal, &data)?;
        }
        if event::poll(Duration::from_millis(200))? {
            if let CEvent::Key(KeyEvent { code, .. }) = event::read()? {
                if code == KeyCode::Char('q') {
                    break;
                }
            }
        }
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// =====================
// DEVICE SELECTION TUI (using tui crate)
// =====================

fn draw_device_selection<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &App,
) -> Result<()> {
    terminal.draw(|f| {
        let size = f.size();
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .margin(2)
            .constraints(
                [
                    Constraint::Length(3),
                    Constraint::Min(2),
                    Constraint::Length(3),
                ]
                .as_ref(),
            )
            .split(size);

        // Header: show which device is being selected.
        let title = match app.stage {
            SelectionStage::Hr => "Select Heart Rate Device",
            SelectionStage::Trainer => "Select Trainer Device",
        };
        let header = tui::widgets::Paragraph::new(title).block(
            Block::default()
                .borders(tui::widgets::Borders::ALL)
                .title("Device Selection"),
        );
        f.render_widget(header, chunks[0]);

        // Prepare table rows.
        // Only include devices with a known name (i.e. not "Unknown").
        let rows: Vec<Row> = app
            .devices
            .iter()
            .filter(|(_, name, _, _)| name.to_lowercase() != "unknown")
            .map(|(_stable_id, name, rssi, _)| {
                let (bar, color) = rssi_bar(*rssi);
                Row::new(vec![
                    Cell::from(name.to_string()),
                    Cell::from(rssi.to_string()),
                    // Render the bar with the chosen color.
                    Cell::from(Span::styled(bar, Style::default().fg(color))),
                ])
            })
            .collect();

        let table = Table::new(rows)
            .header(Row::new(vec!["Name", "RSSI", "Signal"]))
            .block(
                Block::default()
                    .borders(tui::widgets::Borders::ALL)
                    .title("Discovered Devices"),
            )
            .highlight_style(Style::default().bg(Color::Blue).fg(Color::White))
            .widths(&[
                Constraint::Percentage(50),
                Constraint::Length(10),
                Constraint::Length(15),
            ]);

        // Create a mutable TableState and set selection.
        let mut table_state = tui::widgets::TableState::default();
        table_state.select(Some(app.selected_index));
        f.render_stateful_widget(table, chunks[1], &mut table_state);

        let footer = tui::widgets::Paragraph::new(
            "Use Up/Down arrows to navigate, Enter to select. (Press 'q' to quit)",
        )
        .block(Block::default().borders(tui::widgets::Borders::ALL));
        f.render_widget(footer, chunks[2]);
    })?;
    Ok(())
}

async fn device_selection_tui() -> Result<(Peripheral, Peripheral)> {
    // Prepare BLE adapter and start scanning.
    let manager = Manager::new().await?;
    let adapters = manager.adapters().await?;
    let adapter = adapters
        .into_iter()
        .next()
        .ok_or(anyhow::anyhow!("No BLE adapters found"))?;
    adapter.start_scan(Default::default()).await?;

    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new();
    let start_scan = Instant::now();
    let mut selection_complete = false;

    // Main event loop for device selection.
    while !selection_complete {
        // Update the device list using stable IDs.
        let peripherals = adapter.peripherals().await?;
        let mut new_devices = Vec::new();
        for p in peripherals {
            let props = p.properties().await?;
            let name = props
                .as_ref()
                .and_then(|p| p.local_name.clone())
                .unwrap_or_else(|| "Unknown".into());
            let rssi = props.as_ref().and_then(|p| p.rssi).unwrap_or(i16::MIN);
            // Skip devices with name "Unknown".
            if name.to_lowercase() == "unknown" {
                continue;
            }
            // Check if this peripheral already exists in app.devices.
            let p_id_str = format!("{:?}", p.id());
            let existing = app
                .devices
                .iter()
                .find(|(_, _, _, existing)| format!("{:?}", existing.id()) == p_id_str);
            let stable_id = if let Some(&(id, _, _, _)) = existing {
                id
            } else {
                let id = app.next_id;
                app.next_id += 1;
                id
            };
            new_devices.push((stable_id, name, rssi, p));
        }
        // Sort devices alphabetically by name.
        new_devices.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        app.devices = new_devices;

        draw_device_selection(&mut terminal, &app)?;

        // Process key events.
        if event::poll(Duration::from_millis(500))? {
            if let CEvent::Key(key) = event::read()? {
                match key.code {
                    KeyCode::Up => {
                        if app.selected_index > 0 {
                            app.selected_index -= 1;
                        }
                    }
                    KeyCode::Down => {
                        if app.selected_index < app.devices.len().saturating_sub(1) {
                            app.selected_index += 1;
                        }
                    }
                    KeyCode::Enter => {
                        if let Some(&(ref _stable_id, ref _name, ref _rssi, ref peripheral)) =
                            app.devices.get(app.selected_index)
                        {
                            match app.stage {
                                SelectionStage::Hr => {
                                    app.hr_device = Some(peripheral.clone());
                                    app.selected_index = 0;
                                    app.stage = SelectionStage::Trainer;
                                }
                                SelectionStage::Trainer => {
                                    app.trainer_device = Some(peripheral.clone());
                                    selection_complete = true;
                                }
                            }
                        }
                    }
                    KeyCode::Char('q') => {
                        disable_raw_mode()?;
                        execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                        terminal.show_cursor()?;
                        return Err(anyhow::anyhow!("User quit during device selection"));
                    }
                    _ => {}
                }
            }
        }
        // Optionally break after 10 seconds if no devices found.
        if start_scan.elapsed() > Duration::from_secs(10) && app.devices.is_empty() {
            break;
        }
    }
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    let hr_device = app
        .hr_device
        .ok_or(anyhow::anyhow!("No Heart Rate device selected"))?;
    let trainer_device = app
        .trainer_device
        .ok_or(anyhow::anyhow!("No Trainer device selected"))?;

    if !hr_device.is_connected().await? {
        hr_device.connect().await?;
    }
    if !trainer_device.is_connected().await? {
        trainer_device.connect().await?;
    }
    hr_device.discover_services().await?;
    trainer_device.discover_services().await?;
    Ok((hr_device, trainer_device))
}

// =====================
// MAIN FUNCTION
// =====================

#[tokio::main]
async fn main() -> Result<()> {
    // Run the device selection TUI.
    let (hr_device, trainer_device) = device_selection_tui().await?;
    println!("Devices selected.");

    let shared_state = Arc::new(Mutex::new(SharedState::new()));

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

    live_data_tui(shared_state).await?;

    let _ = tokio::join!(hr_task, trainer_task);
    Ok(())
}
