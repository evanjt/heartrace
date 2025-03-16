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
use std::cmp::Reverse;
use std::io::{Write, stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
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

struct SharedState {
    current_hr: u32,
    current_power: u32, // Measured power from trainer notifications.
    cadence: u32,
    target_hr: u32,
    target_power: u32,    // Computed ideal power based on HR error.
    last_sent_power: u32, // The last power value “sent” to the trainer.
    start_time: Instant,
    trainer_raw_data: String, // Latest raw data string from the trainer.
}

impl SharedState {
    fn new() -> Self {
        Self {
            current_hr: 0,
            current_power: 0,
            cadence: 0,
            target_hr: 140,
            target_power: 0,
            last_sent_power: 0,
            start_time: Instant::now(),
            trainer_raw_data: String::new(),
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
    // Devices stored with a stable local id, name, RSSI, and Peripheral.
    devices: Vec<(u32, String, i16, Peripheral)>,
    selected_index: usize,
    stage: SelectionStage,
    hr_device: Option<Peripheral>,
    trainer_device: Option<Peripheral>,
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

/// Computes a 10-character wide signal strength bar and a color based on the RSSI.
/// Excellent = -40, Poor = -100.
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

/// Subscribes to HR notifications using the standard HR Measurement UUID.
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
            let mut s = state.lock().await;
            s.current_hr = hr;
        }
    }
    Ok(())
}

/// Subscribes to Trainer notifications.
/// Here we try interpreting the data using an alternative parsing:
/// interpret bytes 2-3 as instantaneous power (e.g. in deci-watts)
/// and byte 4 as cadence. The raw data is stored for display.
async fn run_trainer_notifications(
    trainer_device: Peripheral,
    state: Arc<Mutex<SharedState>>,
) -> Result<()> {
    let bike_uuid = Uuid::parse_str("00002AD2-0000-1000-8000-00805F9B34FB")?;
    let characteristics = trainer_device.characteristics();
    let available_chars: Vec<String> = characteristics.iter().map(|c| c.uuid.to_string()).collect();
    let bike_data_char = characteristics.iter().find(|c| c.uuid == bike_uuid);

    if let Some(charac) = bike_data_char {
        trainer_device.subscribe(charac).await?;
        let mut notif_stream = trainer_device.notifications().await?;
        while let Some(data) = notif_stream.next().await {
            let raw = data
                .value
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect::<Vec<_>>()
                .join(" ");
            {
                let mut s = state.lock().await;
                s.trainer_raw_data = raw.clone();

                if data.value.len() >= 8 {
                    // FTMS Indoor Bike Data format:
                    // Bytes 0-1: Flags, 2-3: Instantaneous Speed, 4-5: Instantaneous Cadence, 6-7: Instantaneous Power.
                    let _flags = u16::from_le_bytes([data.value[0], data.value[1]]);
                    // let speed = u16::from_le_bytes([data.value[2], data.value[3]]) as f32 / 100.0;
                    let cadence = u16::from_le_bytes([data.value[4], data.value[5]]) as u32 / 2;
                    let power = i16::from_le_bytes([data.value[6], data.value[7]]) as i32;
                    s.current_power = power.abs() as u32; // use abs() in case power is signed.
                    s.cadence = cadence;
                }
            }
        }
    } else {
        let err_msg = format!(
            "Indoor Bike Data characteristic not found. Available: {:?}",
            available_chars
        );
        {
            let mut s = state.lock().await;
            s.trainer_raw_data = err_msg.clone();
        }
        return Err(anyhow::anyhow!(err_msg));
    }
    Ok(())
}

async fn set_trainer_target_power(trainer_device: &Peripheral, power: u16) -> Result<()> {
    let control_point_uuid = Uuid::parse_str("00002AD9-0000-1000-8000-00805F9B34FB")?;

    let characteristics = trainer_device.characteristics();
    let characteristic = characteristics
        .iter()
        .find(|c| c.uuid == control_point_uuid)
        .ok_or(anyhow::anyhow!(
            "FTMS Control Point characteristic not found"
        ))?;

    let power_bytes = power.to_le_bytes();
    let command = vec![0x05, power_bytes[0], power_bytes[1]];

    trainer_device
        .write(
            characteristic,
            &command,
            btleplug::api::WriteType::WithResponse,
        )
        .await?;

    println!("Sent power command: {} W", power);
    Ok(())
}

// =====================
// CONTROL LOOP & CSV LOGGING
// =====================

/// Adjusts target power based on HR error using a simple proportional controller.
/// Updates both the "ideal" power (target_power) and simulates sending it by updating last_sent_power.
async fn control_loop(trainer_device: Peripheral, state: Arc<Mutex<SharedState>>) {
    let mut interval = time::interval(Duration::from_secs(2));
    let k: f32 = 1.0;

    loop {
        interval.tick().await;
        {
            let mut data = state.lock().await;
            let error = data.target_hr as i32 - data.current_hr as i32;
            let adjustment = (k * error as f32).round() as i32;
            let new_power = (data.target_power as i32 + adjustment).clamp(0, 400) as u32;
            data.target_power = new_power;
            data.last_sent_power = new_power;
        }
        if let Err(e) = set_trainer_target_power(&trainer_device, {
            let data = state.lock().await;
            data.target_power as u16
        })
        .await
        {
            eprintln!("Failed to send power command: {:?}", e);
        }
    }
}

/// Logs workout data to CSV every second.
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
        let data = state.lock().await;
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
        // Split the screen horizontally: 60% for metrics, 40% for raw trainer data.
        let chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)].as_ref())
            .split(size);

        let left_block = Block::default()
            .borders(tui::widgets::Borders::ALL)
            .title("Metrics");
        let elapsed = state.start_time.elapsed().as_secs();
        let left_text = vec![
            Spans::from(Span::raw(format!("Elapsed Time: {} sec", elapsed))),
            Spans::from(Span::raw(format!("Current HR: {} bpm", state.current_hr))),
            Spans::from(Span::raw(format!(
                "Current Power: {} W",
                state.current_power
            ))),
            Spans::from(Span::raw(format!("Cadence: {} rpm", state.cadence))),
            Spans::from(Span::raw(format!("Target HR: {} bpm", state.target_hr))),
            Spans::from(Span::raw(format!("Ideal Power: {} W", state.target_power))),
            Spans::from(Span::raw(format!(
                "Last Sent Power: {} W",
                state.last_sent_power
            ))),
        ];
        let left_paragraph = tui::widgets::Paragraph::new(left_text).block(left_block);
        f.render_widget(left_paragraph, chunks[0]);

        let right_block = Block::default()
            .borders(tui::widgets::Borders::ALL)
            .title("Trainer Raw Data");
        let right_text = vec![
            Spans::from(Span::raw(state.trainer_raw_data.clone())),
            Spans::from(Span::raw("Press 'q' to quit.")),
        ];
        let right_paragraph = tui::widgets::Paragraph::new(right_text).block(right_block);
        f.render_widget(right_paragraph, chunks[1]);
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
            let data = state.lock().await;
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

        let rows: Vec<Row> = app
            .devices
            .iter()
            .map(|(_id, name, rssi, _)| {
                let (bar, color) = rssi_bar(*rssi);
                Row::new(vec![
                    Cell::from(name.to_string()),
                    Cell::from(rssi.to_string()),
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

    while !selection_complete {
        let peripherals = adapter.peripherals().await?;
        let mut new_devices = Vec::new();
        for p in peripherals {
            let props = p.properties().await?;
            let name = props
                .as_ref()
                .and_then(|p| p.local_name.clone())
                .unwrap_or_else(|| "Unknown".into());
            let rssi = props.as_ref().and_then(|p| p.rssi).unwrap_or(i16::MIN);
            if name.to_lowercase() == "unknown" {
                continue;
            }
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
        new_devices.sort_by(|a, b| a.1.to_lowercase().cmp(&b.1.to_lowercase()));
        app.devices = new_devices;

        draw_device_selection(&mut terminal, &app)?;

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
                        if let Some((_id, _name, _rssi, peripheral)) =
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
    let trainer_for_control = trainer_device.clone();
    let trainer_task = tokio::spawn(async move {
        if let Err(e) = run_trainer_notifications(trainer_device, trainer_state).await {
            eprintln!("Trainer notifications error: {:?}", e);
        }
    });

    let control_state = shared_state.clone();
    tokio::spawn(async move {
        control_loop(trainer_for_control, control_state).await;
    });

    live_data_tui(shared_state).await?;

    let _ = tokio::join!(hr_task, trainer_task);
    Ok(())
}
