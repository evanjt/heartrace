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
use serde::{Deserialize, Serialize};
use std::cmp::Reverse;
use std::fs::OpenOptions;
use std::io::{Write, stdin, stdout};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, broadcast};
use tokio::time;
use tui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Span, Spans},
    widgets::{Axis, Block, Cell, Chart, Dataset, Row, Table},
};
use uuid::Uuid;

// =====================
// CONFIGURATION
// =====================

#[derive(Serialize, Deserialize, Debug)]
struct Config {
    target_hr: u32,                    // desired heart rate
    ftp: u32,                          // Functional Threshold Power (max power), in Watts
    ramp_rate: u32,                    // max power change per control loop iteration (W)
    hr_device_id: Option<String>,      // previously selected HR device id
    trainer_device_id: Option<String>, // previously selected trainer device id
}

impl Default for Config {
    fn default() -> Self {
        Self {
            target_hr: 140,
            ftp: 300,
            ramp_rate: 5,
            hr_device_id: None,
            trainer_device_id: None,
        }
    }
}

fn load_config() -> Result<Config> {
    let config_str = std::fs::read_to_string("config.toml").unwrap_or_default();
    if config_str.trim().is_empty() {
        Ok(Config::default())
    } else {
        let config: Config = toml::from_str(&config_str)?;
        Ok(config)
    }
}

fn save_config(config: &Config) -> Result<()> {
    let toml_str = toml::to_string_pretty(config)?;
    std::fs::write("config.toml", toml_str)?;
    Ok(())
}

// =====================
// HISTORY DATA STRUCTURE
// =====================

struct HistoryPoint {
    time: u64, // seconds since start
    heart_rate: u32,
    power: u32,
    cadence: u32,
    target_power: u32,
}

// =====================
// SHARED STATE
// =====================

struct SharedState {
    current_hr: u32,
    current_power: u32, // measured power from trainer notifications
    cadence: u32,
    target_hr: u32,
    target_power: u32,    // computed ideal target power
    last_sent_power: u32, // last power command sent
    ftp: u32,             // Functional Threshold Power
    ramp_rate: u32,       // max change per control loop iteration (W)
    start_time: Instant,
    history: Vec<HistoryPoint>, // history over last 60 sec
}

impl SharedState {
    fn new(config: &Config) -> Self {
        Self {
            current_hr: 0,
            current_power: 0,
            cadence: 0,
            target_hr: config.target_hr,
            target_power: 0,
            last_sent_power: 0,
            ftp: config.ftp,
            ramp_rate: config.ramp_rate,
            start_time: Instant::now(),
            history: Vec::new(),
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
    ideal_power: u32,
}

// =====================
// DEVICE SELECTION TUI STRUCTURES
// =====================

#[derive(PartialEq)]
enum SelectionStage {
    Hr,
    Trainer,
}

struct AppTUI {
    devices: Vec<(u32, String, i16, Peripheral)>,
    selected_index: usize,
    stage: SelectionStage,
    hr_device: Option<Peripheral>,
    trainer_device: Option<Peripheral>,
    next_id: u32,
}

impl AppTUI {
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

async fn run_hr_notifications(
    hr_device: Peripheral,
    state: Arc<Mutex<SharedState>>,
    mut shutdown: broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let hr_uuid = Uuid::parse_str("00002A37-0000-1000-8000-00805F9B34FB")?;
    let hr_char = hr_device
        .characteristics()
        .into_iter()
        .find(|c| c.uuid == hr_uuid)
        .ok_or(anyhow::anyhow!("HR characteristic not found"))?;
    hr_device.subscribe(&hr_char).await?;
    let mut notif_stream = hr_device.notifications().await?;
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            maybe_data = notif_stream.next() => {
                if let Some(data) = maybe_data {
                    if data.value.len() >= 2 {
                        let flags = data.value[0];
                        let hr = if flags & 0x01 == 0 {
                            data.value[1] as u32
                        } else if data.value.len() >= 3 {
                            u16::from_le_bytes([data.value[1], data.value[2]]) as u32
                        } else { 0 };
                        let mut s = state.lock().await;
                        s.current_hr = hr;
                    }
                } else { break; }
            }
        }
    }
    Ok(())
}

async fn run_trainer_notifications(
    trainer_device: Peripheral,
    state: Arc<Mutex<SharedState>>,
    mut shutdown: broadcast::Receiver<()>,
) -> anyhow::Result<()> {
    let bike_uuid = Uuid::parse_str("00002AD2-0000-1000-8000-00805F9B34FB")?;
    let characteristics = trainer_device.characteristics();
    let available_chars: Vec<String> = characteristics.iter().map(|c| c.uuid.to_string()).collect();
    let bike_data_char = characteristics.iter().find(|c| c.uuid == bike_uuid);
    if let Some(charac) = bike_data_char {
        trainer_device.subscribe(charac).await?;
        let mut notif_stream = trainer_device.notifications().await?;
        loop {
            tokio::select! {
                _ = shutdown.recv() => break,
                maybe_data = notif_stream.next() => {
                    if let Some(data) = maybe_data {
                        let mut s = state.lock().await;
                        if data.value.len() >= 8 {
                            let cadence = u16::from_le_bytes([data.value[4], data.value[5]]) as u32 / 2;
                            let power = i16::from_le_bytes([data.value[6], data.value[7]]) as i32;
                            s.current_power = power.unsigned_abs();
                            s.cadence = cadence;
                        }
                    } else { break; }
                }
            }
        }
    } else {
        let err_msg = format!(
            "Indoor Bike Data characteristic not found. Available: {:?}",
            available_chars
        );
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
    Ok(())
}

// =====================
// CONTROL LOOP & CSV LOGGING
// =====================

async fn control_loop(
    trainer_device: Peripheral,
    state: Arc<Mutex<SharedState>>,
    mut shutdown: broadcast::Receiver<()>,
) {
    let mut interval = time::interval(Duration::from_secs(2));
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = interval.tick() => {
                let target_power = {
                    let mut data = state.lock().await;
                    let hr_error = data.target_hr as i32 - data.current_hr as i32;
                    let delta = if hr_error > 3 { data.ramp_rate as i32 }
                        else if hr_error < -3 { -(data.ramp_rate as i32) }
                        else { 0 };
                    let new_power = (data.target_power as i32 + delta).clamp(0, data.ftp as i32) as u32;
                    data.target_power = new_power;
                    data.last_sent_power = new_power;
                    new_power as u16
                };
                if let Err(e) = set_trainer_target_power(&trainer_device, target_power).await {
                    eprintln!("Failed to send power command: {:?}", e);
                }
            }
        }
    }
}
async fn csv_logging_task(
    state: Arc<Mutex<SharedState>>,
    mut shutdown: broadcast::Receiver<()>,
) -> Result<()> {
    let file_path = "workout.csv";
    let mut wtr = csv::Writer::from_writer(
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(file_path)?,
    );
    let mut interval = time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            _ = shutdown.recv() => break,
            _ = interval.tick() => {
                // Destructure values from the state in a temporary scope.
                let (elapsed_sec, current_hr, current_power, cadence, target_hr, ideal_power) = {
                    let data = state.lock().await;
                    (
                        data.start_time.elapsed().as_secs(),
                        data.current_hr,
                        data.current_power,
                        data.cadence,
                        data.target_hr,
                        data.target_power,
                    )
                };
                {
                    let mut data = state.lock().await;
                    data.history.push(HistoryPoint {
                        time: elapsed_sec,
                        heart_rate: current_hr,
                        power: current_power,
                        cadence,
                        target_power: ideal_power,
                    });
                    while let Some(first) = data.history.first() {
                        if elapsed_sec - first.time > 60 {
                            data.history.remove(0);
                        } else {
                            break;
                        }
                    }
                }
                let record = LogRecord {
                    timestamp: Local::now().to_rfc3339(),
                    elapsed_sec,
                    heart_rate: current_hr,
                    power: current_power,
                    cadence,
                    target_hr,
                    ideal_power,
                };
                wtr.serialize(record)?;
                wtr.flush()?;
            }
        }
    }
    Ok(())
}

// =====================
// LIVE DATA TUI (Dashboard)
// =====================

fn draw_live_dashboard<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    state: &SharedState,
) -> Result<(), anyhow::Error> {
    terminal
        .draw(|f| {
            let size = f.size();
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(8), Constraint::Min(10)].as_ref())
                .split(size);
            let top_block = Block::default()
                .borders(tui::widgets::Borders::ALL)
                .title("Metrics");
            let elapsed = state.start_time.elapsed().as_secs();
            let top_text = vec![
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
            let top_paragraph = tui::widgets::Paragraph::new(top_text).block(top_block);
            f.render_widget(top_paragraph, chunks[0]);

            let x_max = state.start_time.elapsed().as_secs() as f64;
            let x_min = if x_max > 60.0 { x_max - 60.0 } else { 0.0 };
            let y_max = state.ftp as f64;
            let hr_data: Vec<(f64, f64)> = state
                .history
                .iter()
                .map(|p| (p.time as f64, p.heart_rate as f64))
                .collect();
            let cadence_data: Vec<(f64, f64)> = state
                .history
                .iter()
                .map(|p| (p.time as f64, p.cadence as f64))
                .collect();
            let power_data: Vec<(f64, f64)> = state
                .history
                .iter()
                .map(|p| (p.time as f64, p.power as f64))
                .collect();
            let target_power_data: Vec<(f64, f64)> = state
                .history
                .iter()
                .map(|p| (p.time as f64, p.target_power as f64))
                .collect();
            let datasets = vec![
                Dataset::default()
                    .name("HR")
                    .marker(tui::symbols::Marker::Dot)
                    .style(Style::default().fg(Color::Red))
                    .data(&hr_data),
                Dataset::default()
                    .name("Cadence")
                    .marker(tui::symbols::Marker::Braille)
                    .style(Style::default().fg(Color::Green))
                    .data(&cadence_data),
                Dataset::default()
                    .name("Power")
                    .marker(tui::symbols::Marker::Dot)
                    .style(Style::default().fg(Color::Blue))
                    .data(&power_data),
                Dataset::default()
                    .name("Ideal Power")
                    .marker(tui::symbols::Marker::Braille)
                    .style(Style::default().fg(Color::Magenta))
                    .data(&target_power_data),
            ];
            let chart = Chart::new(datasets)
                .block(
                    Block::default()
                        .title("History (Last 60 sec)")
                        .borders(tui::widgets::Borders::ALL),
                )
                .x_axis(
                    Axis::default()
                        .title("Time (s)")
                        .style(Style::default().fg(Color::Gray))
                        .bounds([x_min, x_max])
                        .labels(vec![
                            Span::raw(format!("{:.0}", x_min)),
                            Span::raw(format!("{:.0}", (x_min + x_max) / 2.0)),
                            Span::raw(format!("{:.0}", x_max)),
                        ]),
                )
                .y_axis(
                    Axis::default()
                        .title("Value")
                        .style(Style::default().fg(Color::Gray))
                        .bounds([0.0, y_max])
                        .labels(vec![
                            Span::raw("0"),
                            Span::raw(format!("{:.0}", y_max / 2.0)),
                            Span::raw(format!("{:.0}", y_max)),
                        ]),
                );
            f.render_widget(chart, chunks[1]);
        })
        .map(|_| ())
        .map_err(anyhow::Error::from)
}

async fn live_data_tui(
    state: Arc<Mutex<SharedState>>,
    shutdown: broadcast::Sender<()>,
) -> Result<()> {
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
                    let _ = shutdown.send(());
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
// DEVICE SELECTION TUI
// =====================
fn draw_device_selection<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &AppTUI,
) -> Result<(), anyhow::Error> {
    terminal
        .draw(|f| {
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
        })
        .map(|_| ())
        .map_err(anyhow::Error::from)
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
    let mut app = AppTUI::new();
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
// START SCREEN & SETTINGS
// =====================
fn draw_start_screen<B: tui::backend::Backend>(
    terminal: &mut Terminal<B>,
    config: &Config,
    hr_status: &str,
    trainer_status: &str,
) -> Result<(), anyhow::Error> {
    terminal
        .draw(|f| {
            let size = f.size();
            let block = Block::default()
                .borders(tui::widgets::Borders::ALL)
                .title("HearTrace - Start Screen");
            f.render_widget(block, size);
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .margin(2)
                .constraints(
                    [
                        Constraint::Length(7),
                        Constraint::Length(3),
                        Constraint::Min(3),
                    ]
                    .as_ref(),
                )
                .split(size);
            let config_text = vec![
                Spans::from(Span::raw(format!("Target HR: {} bpm", config.target_hr))),
                Spans::from(Span::raw(format!("FTP: {} W", config.ftp))),
                Spans::from(Span::raw(format!("Ramp Rate: {} W", config.ramp_rate))),
                Spans::from(Span::raw(format!(
                    "HR Device ID: {}",
                    config.hr_device_id.as_deref().unwrap_or("Not set")
                ))),
                Spans::from(Span::raw(format!(
                    "Trainer Device ID: {}",
                    config.trainer_device_id.as_deref().unwrap_or("Not set")
                ))),
            ];
            let status_text = vec![
                Spans::from(Span::raw(format!("HR Device Status: {}", hr_status))),
                Spans::from(Span::raw(format!(
                    "Trainer Device Status: {}",
                    trainer_status
                ))),
            ];
            let options_text = vec![Spans::from(Span::raw(
                "Options: [l] Live  [s] Settings  [d] Devices  [q] Quit",
            ))];
            let config_paragraph = tui::widgets::Paragraph::new(config_text).block(
                Block::default()
                    .borders(tui::widgets::Borders::ALL)
                    .title("Configuration"),
            );
            let status_paragraph = tui::widgets::Paragraph::new(status_text).block(
                Block::default()
                    .borders(tui::widgets::Borders::ALL)
                    .title("Device Status"),
            );
            let options_paragraph = tui::widgets::Paragraph::new(options_text)
                .block(Block::default().borders(tui::widgets::Borders::ALL));
            f.render_widget(config_paragraph, chunks[0]);
            f.render_widget(status_paragraph, chunks[1]);
            f.render_widget(options_paragraph, chunks[2]);
        })
        .map(|_| ())
        .map_err(anyhow::Error::from)
}

async fn start_screen(config: &Config) -> Result<char> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    loop {
        let hr_status = match lookup_device_status(&config.hr_device_id).await {
            Ok(status) => status,
            Err(_) => "Error".to_string(),
        };
        let trainer_status = match lookup_device_status(&config.trainer_device_id).await {
            Ok(status) => status,
            Err(_) => "Error".to_string(),
        };
        draw_start_screen(&mut terminal, config, &hr_status, &trainer_status)?;
        if event::poll(Duration::from_millis(500))? {
            if let CEvent::Key(KeyEvent { code, .. }) = event::read()? {
                disable_raw_mode()?;
                execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
                terminal.show_cursor()?;
                if let KeyCode::Char(c) = code {
                    return Ok(c);
                }
            }
        }
    }
}

fn settings_page(config: &mut Config) -> Result<()> {
    println!("--- Settings ---");
    println!("Current Target HR (bpm): {}", config.target_hr);
    println!("Enter new Target HR (or press Enter to keep current):");
    let mut input = String::new();
    stdin().read_line(&mut input)?;
    if let Ok(new_val) = input.trim().parse() {
        config.target_hr = new_val;
    }
    input.clear();
    println!("Current FTP (W): {}", config.ftp);
    println!("Enter new FTP (or press Enter to keep current):");
    stdin().read_line(&mut input)?;
    if let Ok(new_val) = input.trim().parse() {
        config.ftp = new_val;
    }
    input.clear();
    println!("Current Ramp Rate (W per interval): {}", config.ramp_rate);
    println!("Enter new Ramp Rate (or press Enter to keep current):");
    stdin().read_line(&mut input)?;
    if let Ok(new_val) = input.trim().parse() {
        config.ramp_rate = new_val;
    }
    println!("Settings updated. Press Enter to continue.");
    input.clear();
    stdin().read_line(&mut input)?;
    Ok(())
}

async fn lookup_device_status(device_id: &Option<String>) -> std::io::Result<String> {
    if let Some(id_str) = device_id {
        if let Ok(manager) = Manager::new().await {
            if let Ok(adapters) = manager.adapters().await {
                if let Some(adapter) = adapters.into_iter().next() {
                    let _ = adapter.start_scan(Default::default()).await;
                    time::sleep(Duration::from_secs(2)).await;
                    if let Ok(peripherals) = adapter.peripherals().await {
                        for p in peripherals {
                            if p.id().to_string() == *id_str {
                                return Ok("Found".into());
                            }
                        }
                    }
                }
            }
        }
        Ok("Not found".into())
    } else {
        Ok("Not set".into())
    }
}

// =====================
// MAIN STATE MACHINE
// =====================

#[tokio::main]
async fn main() -> Result<()> {
    let mut config = load_config().unwrap_or_default();
    println!(
        "Loaded config: target_hr={}, ftp={}, ramp_rate={}",
        config.target_hr, config.ftp, config.ramp_rate
    );

    loop {
        let choice = start_screen(&config).await?;
        match choice {
            'l' => break, // Launch live dashboard.
            's' => {
                disable_raw_mode()?;
                println!();
                settings_page(&mut config)?;
                save_config(&config)?;
            }
            'd' => {
                let (hr_device, trainer_device) = device_selection_tui().await?;
                config.hr_device_id = Some(hr_device.id().to_string());
                config.trainer_device_id = Some(trainer_device.id().to_string());
                save_config(&config)?;
            }
            'q' => return Ok(()),
            _ => {}
        }
    }

    // Re-select devices (or use saved ones) for live dashboard.
    let (hr_device, trainer_device) = device_selection_tui().await?;
    config.hr_device_id = Some(hr_device.id().to_string());
    config.trainer_device_id = Some(trainer_device.id().to_string());
    save_config(&config)?;

    let shared_state = Arc::new(Mutex::new(SharedState::new(&config)));
    let (shutdown_tx, _) = broadcast::channel::<()>(1);

    let hr_state = shared_state.clone();
    let mut hr_shutdown = shutdown_tx.subscribe();
    let hr_task = tokio::spawn(async move {
        if let Err(e) = run_hr_notifications(hr_device, hr_state, hr_shutdown).await {
            eprintln!("HR notifications error: {:?}", e);
        }
    });

    let trainer_state = shared_state.clone();
    let mut trainer_shutdown = shutdown_tx.subscribe();
    let trainer_for_notifications = trainer_device.clone();
    let trainer_task = tokio::spawn(async move {
        if let Err(e) =
            run_trainer_notifications(trainer_for_notifications, trainer_state, trainer_shutdown)
                .await
        {
            eprintln!("Trainer notifications error: {:?}", e);
        }
    });

    let mut control_shutdown = shutdown_tx.subscribe();
    let control_state = shared_state.clone();
    let trainer_for_control = trainer_device.clone();
    tokio::spawn(async move {
        control_loop(trainer_for_control, control_state, control_shutdown).await;
    });

    let mut logging_shutdown = shutdown_tx.subscribe();
    let logging_state = shared_state.clone();
    tokio::spawn(async move {
        if let Err(e) = csv_logging_task(logging_state, logging_shutdown).await {
            eprintln!("CSV logging error: {:?}", e);
        }
    });

    live_data_tui(shared_state, shutdown_tx.clone()).await?;
    let _ = tokio::join!(hr_task, trainer_task);
    Ok(())
}
