use aes::Aes128;
use aes::cipher::{KeyIvInit, StreamCipher};
use chrono::{DateTime, Datelike, Duration, Local};
use ctr::Ctr128BE;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
type Aes128Ctr = Ctr128BE<Aes128>;
use color_eyre::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    buffer::Buffer,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        block::Title,
        calendar::{CalendarEventStore, Monthly},
        Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Widget, Wrap,
    },
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};
use std::time::Duration as StdDuration;
use std::fs;
use std::io::{self, stdout};
use std::path::PathBuf;
use time::Date as TimeDate;

// ─────────────────────────────────────────────
// CHAT MODULE  (AES-128-CTR encrypted relay)
// ─────────────────────────────────────────────

/// Shared 16-byte AES key (must match on both ends).
/// In a real deployment you would do a proper key-exchange (e.g. DH/ECDH).
/// For this course demo we use a compile-time pre-shared key.
const CHAT_KEY: &[u8; 16] = b"SuperSecret1234!";
/// Fixed 16-byte nonce/IV.  CTR mode is safe to reuse a nonce only when the
/// key stream is never reused for different plaintexts.  For a classroom demo
/// we accept this limitation; a production system would generate a random IV
/// per message and prepend it.
const CHAT_IV: &[u8; 16] = b"InitVector123456";

/// Encrypt (or decrypt – CTR is symmetric) a UTF-8 string.
fn aes_ctr_transform(data: &[u8]) -> Vec<u8> {
    let mut cipher = Aes128Ctr::new(CHAT_KEY.into(), CHAT_IV.into());
    let mut buf = data.to_vec();
    cipher.apply_keystream(&mut buf);
    buf
}

/// Encode bytes as lowercase hex.
fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

/// Decode a hex string back to bytes.  Returns None on invalid input.
fn from_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).ok())
        .collect()
}

/// Encrypt a plaintext string → hex-encoded ciphertext.
fn encrypt_msg(plaintext: &str) -> String {
    to_hex(&aes_ctr_transform(plaintext.as_bytes()))
}

/// Decrypt a hex-encoded ciphertext → plaintext string (lossy UTF-8).
fn decrypt_msg(hex_cipher: &str) -> String {
    if let Some(bytes) = from_hex(hex_cipher) {
        let plain = aes_ctr_transform(&bytes);
        String::from_utf8_lossy(&plain).into_owned()
    } else {
        // Not an encrypted payload (e.g. server INFO/ERROR lines) – show as-is
        hex_cipher.to_string()
    }
}

// ─── Chat state ───────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum ChatConnectionState {
    Disconnected,
    Connected,
    Registered(String), // holds our username
}

struct ChatState {
    connection_state: ChatConnectionState,
    /// Lines shown in the chat log (already decrypted / formatted)
    messages: Vec<(String, Color)>,
    /// Text the user is currently typing
    input: String,
    /// Channels for sending raw protocol lines to the background writer thread
    tx: Option<Sender<String>>,
    /// Incoming decoded lines from the background reader thread
    rx: Option<Receiver<String>>,
}

impl ChatState {
    fn new() -> Self {
        ChatState {
            connection_state: ChatConnectionState::Disconnected,
            messages: Vec::new(),
            input: String::new(),
            tx: None,
            rx: None,
        }
    }

    fn push_msg(&mut self, text: impl Into<String>, color: Color) {
        let mut msgs = std::mem::take(&mut self.messages);
        msgs.push((text.into(), color));
        // Keep the last 200 lines so memory stays bounded
        if msgs.len() > 200 {
            msgs.drain(0..msgs.len() - 200);
        }
        self.messages = msgs;
    }

    /// Try to connect to the relay server in background threads.
    /// Returns Ok on successful TCP connect, Err with a message otherwise.
    fn connect(&mut self, host: &str, port: u16) -> Result<(), String> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr)
            .map_err(|e| format!("TCP connect failed: {}", e))?;

        // Clone for writer thread
        let stream_write = stream.try_clone()
            .map_err(|e| format!("Stream clone failed: {}", e))?;

        // Channel: app  →  writer thread
        let (tx_send, rx_send): (Sender<String>, Receiver<String>) = mpsc::channel();
        // Channel: reader thread  →  app
        let (tx_recv, rx_recv): (Sender<String>, Receiver<String>) = mpsc::channel();

        // ── Writer thread ──────────────────────────────────────────────────
        std::thread::spawn(move || {
            let mut writer = stream_write;
            for line in rx_send {
                let data = format!("{}\n", line);
                if writer.write_all(data.as_bytes()).is_err() {
                    break;
                }
            }
        });

        // ── Reader thread ──────────────────────────────────────────────────
        std::thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                match line {
                    Ok(l) => {
                        if tx_recv.send(l).is_err() {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        self.tx = Some(tx_send);
        self.rx = Some(rx_recv);
        self.connection_state = ChatConnectionState::Connected;
        Ok(())
    }

    /// Send a raw protocol line to the server (no encryption wrapper).
    fn send_raw(&self, line: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(line.to_string());
        }
    }

    /// Send an encrypted MSG command.
    fn send_msg(&self, recipient: &str, plaintext: &str) {
        let cipher_hex = encrypt_msg(plaintext);
        self.send_raw(&format!("MSG {} {}", recipient, cipher_hex));
    }

    /// Drain any pending lines from the reader thread and process them.
    fn poll_incoming(&mut self) {
        let lines: Vec<String> = if let Some(rx) = &self.rx {
            rx.try_iter().collect()
        } else {
            return;
        };

        for line in lines {
            self.process_server_line(&line);
        }
    }

    fn process_server_line(&mut self, line: &str) {
        if let Some(rest) = line.strip_prefix("INFO ") {
            // Check for registration confirmation
            if let Some(name) = rest.strip_prefix("Registered as ") {
                self.connection_state =
                    ChatConnectionState::Registered(name.trim().to_string());
            }
            self.push_msg(format!("[server] {}", rest), Color::DarkGray);
        } else if let Some(rest) = line.strip_prefix("ERROR ") {
            self.push_msg(format!("[error] {}", rest), Color::Red);
        } else if let Some(rest) = line.strip_prefix("USERLIST ") {
            self.push_msg(format!("[online] {}", rest), Color::Cyan);
        } else if let Some(rest) = line.strip_prefix("FROM ") {
            // "FROM <sender> <hex_ciphertext>"
            let mut parts = rest.splitn(2, ' ');
            let sender = parts.next().unwrap_or("?");
            let payload = parts.next().unwrap_or("");
            let plaintext = decrypt_msg(payload);
            self.push_msg(format!("{}: {}", sender, plaintext), Color::Green);
        } else {
            self.push_msg(line.to_string(), Color::White);
        }
    }
}

// ─────────────────────────────────────────────
fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let app_result = App::new().run(&mut terminal);

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    app_result
}

#[derive(Serialize, Deserialize, Clone)]
struct TimeEntry {
    activity: String,
    start: DateTime<Local>,
    end: Option<DateTime<Local>>,
    target_duration: Option<Duration>,
}

impl TimeEntry {
    //Format time remainig for countdown
    fn format_countdown(&self) -> String {
        if let Some(target) = self.target_duration {
            let elapsed = self.duration();
            let remaining = target - elapsed;

            if remaining.num_seconds() <= 0 {
                return "DONE!".to_string();
            }

            let hours = remaining.num_hours();
            let minutes = remaining.num_minutes() % 60;
            let seconds = remaining.num_seconds() % 60;
            format!("{}h {}m {}s", hours, minutes, seconds)
        } else {
            self.format_duration()
        }
    }

    //Check if entry is a countdown
    fn is_countdown(&self) -> bool {
        self.target_duration.is_some()
    }

    fn is_countdown_complete(&self) -> bool {
        if let Some(target) = self.target_duration {
            let elapsed = self.duration();
            let remainig = target - elapsed;
            remainig.num_seconds() <= 0
        } else {
            false
        }
    }

    fn remainig_duration(&self) -> Option<Duration> {
        self.target_duration.map(|target| {
            let elapsed = self.duration();
            target - elapsed
        })
    }

    fn duration(&self) -> Duration {
        match self.end {
            Some(end) => end.signed_duration_since(self.start),
            None => Local::now().signed_duration_since(self.start),
        }
    }

    fn format_duration(&self) -> String {
        let duration = self.duration();
        let hours = duration.num_hours();
        let minutes = duration.num_minutes() % 60;
        let seconds = duration.num_seconds() % 60;
        format!("{}h {}m {}s", hours, minutes, seconds)
    }
}

#[derive(Serialize, Deserialize)]
struct TimeTracker {
    entries: Vec<TimeEntry>,
    active_entries: Vec<TimeEntry>,
}

impl TimeTracker {
    fn new() -> Self {
        TimeTracker {
            entries: Vec::new(),
            active_entries: Vec::new(),
        }
    }

    fn get_data_file() -> PathBuf {
        let home = std::env::var("HOME")
            .or_else(|_| std::env::var("USERPROFILE"))
            .unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join(".timetracker.json")
    }

    fn load() -> Self {
        let path = Self::get_data_file();
        if path.exists() {
            let content = fs::read_to_string(&path).unwrap_or_default();
            serde_json::from_str(&content).unwrap_or_else(|_| Self::new())
        } else {
            Self::new()
        }
    }

    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = Self::get_data_file();
        let json = serde_json::to_string_pretty(self)?;
        fs::write(path, json)?;
        Ok(())
    }

    fn start(&mut self, name: &str) -> Result<(), String> {
        if self.active_entries.iter().any(|e| e.activity == name) {
            return Err(format!("Task '{}' is already active", name));
        }
        self.active_entries.push(TimeEntry {
            activity: name.to_string(),
            start: Local::now(),
            target_duration: None,
            end: None,
        });
        Ok(())
    }
    fn start_countdown(&mut self, name: &str, minutes: i64) -> Result<(), String> {
        if self.active_entries.iter().any(|e| e.activity == name) {
            return Err(format!("Task '{}' is already active", name));
        }

        self.active_entries.push(TimeEntry {
            activity: name.to_string(),
            start: Local::now(),
            target_duration: Some(Duration::minutes(minutes)),
            end: None,
        });
        Ok(())
    }

    fn stop(&mut self, index: usize) -> Result<String, String> {
        if index >= self.active_entries.len() {
            return Err("Invalid task index".to_string());
        }

        let mut entry = self.active_entries.remove(index);
        let now = Local::now();

        if let Some(target) = entry.target_duration {
            let target_end = entry.start + target;
            if now > target_end {
                entry.end = Some(target_end);
            } else {
                entry.end = Some(now);
            }
        } else {
            entry.end = Some(now);
        }

        let name = entry.activity.clone();
        self.entries.push(entry);
        Ok(name)
    }

    fn stop_all(&mut self) -> usize {
        let count = self.active_entries.len();
        let now = Local::now();

        for mut entry in self.active_entries.drain(..) {
            if let Some(target) = entry.target_duration {
                let target_end = entry.start + target;

                if now > target_end {
                    entry.end = Some(target_end);
                } else {
                    entry.end = Some(now);
                }
            } else {
                entry.end = Some(now);
            }
            self.entries.push(entry);
        }

        count
    }

    fn delete_task(&mut self, index: usize) -> Result<String, String> {
        if index >= self.entries.len() {
            return Err("Invalid task index".to_string());
        }
        let entry = self.entries.remove(index);
        Ok(entry.activity)
    }

    fn delete_all(&mut self) -> usize {
        let count = self.entries.len();
        self.entries.clear();
        count
    }

    fn task_on_date(&self, date: chrono::NaiveDate) -> usize {
        self.entries
            .iter()
            .filter(|entry| {
                if let Some(end) = entry.end {
                    end.date_naive() == date
                } else {
                    false
                }
            })
            .count()
    }

    fn duration_on_date(&self, date: chrono::NaiveDate) -> Duration {
        self.entries
            .iter()
            .filter_map(|entry| {
                if let Some(end) = entry.end {
                    if end.date_naive() == date {
                        return Some(entry.duration());
                    }
                }
                None
            })
            .fold(Duration::zero(), |acc, d| acc + d)
    }

    fn task_in_range(&self, start: chrono::NaiveDate, end: chrono::NaiveDate) -> Vec<&TimeEntry> {
        self.entries
            .iter()
            .filter(|entry| {
                if let Some(entry_end) = entry.end {
                    let entry_date = entry_end.date_naive();
                    entry_date >= start && entry_date <= end
                } else {
                    false
                }
            })
            .collect()
    }
}

enum InputMode {
    Normal,
    StartCountdown,
    StopTask,
    ConfirmDeleteAll,
    DeleteTask,
    SelectDay,
    ChatTyping,       // user is composing a chat message / command
}

enum View {
    Main,
    History,
    Calendar,
    Chat,
}

struct App {
    tracker: TimeTracker,
    exit: bool,
    mode: InputMode,
    view: View,
    input: String,
    message: Option<String>,
    message_color: Color,
    list_state: ListState,
    active_list_state: ListState,
    calendar_date: DateTime<Local>,
    selected_day: Option<chrono::NaiveDate>,
    chat: ChatState,
}

impl App {
    fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));

        let mut active_list_state = ListState::default();
        active_list_state.select(Some(0));

        App {
            tracker: TimeTracker::load(),
            exit: false,
            mode: InputMode::Normal,
            view: View::Main,
            input: String::new(),
            message: None,
            message_color: Color::Green,
            list_state,
            active_list_state,
            calendar_date: Local::now(),
            selected_day: None,
            chat: ChatState::new(),
        }
    }

    pub fn run(&mut self, terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.size();

        match self.view {
            View::Main => self.render_main_view(area, frame.buffer_mut()),
            View::History => self.render_history_view(area, frame.buffer_mut()),
            View::Calendar => self.render_calendar_view(area, frame.buffer_mut()),
            View::Chat => self.render_chat_view(area, frame.buffer_mut()),
        }
    }

    fn handle_countdown_input(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    if let Some((name, minutes_str)) = self.input.split_once(':') {
                        if let Ok(minutes) = minutes_str.parse::<i64>() {
                            if minutes > 0 {
                                match self.tracker.start_countdown(name.trim(), minutes) {
                                    Ok(_) => {
                                        self.message = Some(format!(
                                            "Started countdown: {} ({} min)",
                                            name.trim(),
                                            minutes
                                        ));
                                        self.message_color = Color::Green;
                                        let _ = self.tracker.save();
                                    }
                                    Err(e) => {
                                        self.message = Some(format!("Error: {}", e));
                                        self.message_color = Color::Red;
                                    }
                                }
                            } else {
                                self.message = Some("Minutes must be positive".to_string());
                                self.message_color = Color::Red;
                            }
                        } else {
                            self.message = Some("Invalid minutes format".to_string());
                            self.message_color = Color::Red;
                        }
                    } else {
                        self.message =
                            Some("Format: TaskName:Minutes (e.g., Study:25)".to_string());
                        self.message_color = Color::Red;
                    }
                }
                self.mode = InputMode::Normal;
                self.input.clear();
            }
            KeyCode::Char(e) => {
                self.input.push(e);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.input.clear();
                self.message = Some("Cancelled".to_string());
                self.message_color = Color::Yellow;
            }

            _ => {}
        }
    }

    fn render_main_view(&mut self, area: Rect, buf: &mut Buffer) {
        // Keep this loop to trigger countdown updates
        for entry in self.tracker.active_entries.iter() {
            if entry.is_countdown() {
                let _ = entry.is_countdown_complete();
                let _ = entry.remainig_duration();
            }
        }

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(area);

        let title = Paragraph::new(" ⏱  Time Tracker ")
            .centered()
            .style(Style::default().fg(Color::Magenta).bold())
            .block(Block::bordered().border_style(Style::default().fg(Color::Magenta)));
        title.render(chunks[0], buf);

        let status_text = if self.tracker.active_entries.is_empty() {
            Text::from(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        "No active tasks",
                        Style::default().fg(Color::DarkGray).italic(),
                    ),
                ]),
            ])
        } else {
            let mut lines = vec![
                Line::from(vec![Span::styled(
                    format!(
                        "{} Active Task{}",
                        self.tracker.active_entries.len(),
                        if self.tracker.active_entries.len() == 1 {
                            ""
                        } else {
                            "s"
                        }
                    ),
                    Style::default().fg(Color::Yellow).bold(),
                )]),
                Line::from(""),
            ];

            for (i, entry) in self.tracker.active_entries.iter().enumerate() {
                let duration_text = if entry.is_countdown() {
                    entry.format_countdown()
                } else {
                    entry.format_duration()
                };

                let icon = if entry.is_countdown() {
                    " ⏱ "
                } else {
                    " ● "
                };

                let time_color = if entry.is_countdown() {
                    if entry.is_countdown_complete() {
                        Color::Magenta
                    } else if let Some(remaining) = entry.remainig_duration() {
                        if remaining.num_minutes() < 1 {
                            Color::Red
                        } else if remaining.num_minutes() < 5 {
                            Color::Yellow
                        } else if remaining.num_minutes() < 10 {
                            Color::Green
                        } else {
                            Color::Cyan
                        }
                    } else {
                        Color::Cyan
                    }
                } else {
                    Color::Cyan
                };

                lines.push(Line::from(vec![
                    Span::styled(icon, Style::default().fg(Color::Green).bold()),
                    Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
                    Span::styled(&entry.activity, Style::default().fg(Color::White).bold()),
                    Span::raw(" - "),
                    Span::styled(duration_text, Style::default().fg(time_color).bold()),
                ]));
            }

            Text::from(lines)
        };

        let status_block = Paragraph::new(status_text)
            .block(
                Block::bordered()
                    .title(" Current Status ")
                    .border_style(Style::default().fg(Color::White)),
            )
            .wrap(Wrap { trim: false });
        status_block.render(chunks[1], buf);

        if let Some(ref msg) = self.message {
            let message_block = Paragraph::new(msg.as_str())
                .style(Style::default().fg(self.message_color))
                .block(Block::bordered());
            message_block.render(chunks[2], buf);
        }

        let controls = match self.mode {
            InputMode::Normal => {
                vec![Line::from(vec![
                    Span::styled("X", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop  "),
                    Span::styled("T", Style::default().fg(Color::Magenta).bold()),
                    Span::raw(" Countdown  "),
                    Span::styled("A", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop All  "),
                    Span::styled("C", Style::default().fg(Color::Cyan).bold()),
                    Span::raw(" Calendar  "),
                    Span::styled("H", Style::default().fg(Color::Yellow).bold()),
                    Span::raw(" History  "),
                    Span::styled("M", Style::default().fg(Color::Blue).bold()),
                    Span::raw(" Chat  "),
                    Span::styled("Q", Style::default().fg(Color::Gray).bold()),
                    Span::raw(" Quit "),
                ])]
            }
            InputMode::StartCountdown => {
                vec![
                    Line::from(vec![
                        Span::raw("Task:Minutes (e.g., Study:25): "),
                        Span::styled(&self.input, Style::default().fg(Color::Yellow)),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ]),
                    Line::from(vec![
                        Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                        Span::raw(" to start "),
                        Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                        Span::raw(" to cancel "),
                    ]),
                ]
            }
            InputMode::StopTask => {
                vec![
                    Line::from(vec![
                        Span::raw("Task number to stop: "),
                        Span::styled(&self.input, Style::default().fg(Color::Yellow)),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ]),
                    Line::from(vec![
                        Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                        Span::raw(" to confirm "),
                        Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                        Span::raw(" to cancel"),
                    ]),
                ]
            }
            InputMode::DeleteTask | InputMode::SelectDay => {
                vec![Line::from(vec![
                    Span::styled("S", Style::default().fg(Color::Green).bold()),
                    Span::raw(" Start Task  "),
                    Span::styled("X", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop Task  "),
                    Span::styled("A", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop All  "),
                    Span::styled("H", Style::default().fg(Color::Yellow).bold()),
                    Span::raw(" History  "),
                    Span::styled("Q", Style::default().fg(Color::Gray).bold()),
                    Span::raw(" Quit "),
                ])]
            }
            InputMode::ConfirmDeleteAll => {
                vec![Line::from(vec![
                    Span::styled("Y", Style::default().fg(Color::Green).bold()),
                    Span::raw(" Confirm Delete All  "),
                    Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Cancel  "),
                ])]
            }
            InputMode::ChatTyping => {
                vec![Line::from(vec![
                    Span::styled("I", Style::default().fg(Color::Blue).bold()),
                    Span::raw(" type  "),
                    Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                    Span::raw(" send  "),
                    Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                    Span::raw(" back "),
                ])]
            }
        };
        let controls_block = Paragraph::new(controls)
            .block(Block::bordered().border_style(Style::default().fg(Color::Gray)))
            .centered();
        controls_block.render(chunks[3], buf);
    }

    fn render_history_view(&mut self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(area);

        let title = Paragraph::new("Task History ")
            .centered()
            .style(Style::default().fg(Color::Magenta).bold())
            .block(Block::bordered().border_style(Style::default().fg(Color::Magenta)));
        title.render(chunks[0], buf);

        if self.tracker.entries.is_empty() {
            let empty = Paragraph::new("No completed tasks yet")
                .style(Style::default().fg(Color::DarkGray).italic())
                .block(Block::bordered().title(" Completed Tasks "))
                .centered();
            empty.render(chunks[1], buf);
        } else {
            let items: Vec<ListItem> = self
                .tracker
                .entries
                .iter()
                .enumerate()
                .map(|(i, entry)| {
                    let duration = entry.format_duration();
                    let date = entry.start.format("%m-%d-%Y %H:%M").to_string();

                    let content = Line::from(vec![
                        Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
                        Span::styled(&entry.activity, Style::default().fg(Color::White).bold()),
                        Span::raw(" - "),
                        Span::styled(duration, Style::default().fg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(format!("({})", date), Style::default().fg(Color::DarkGray)),
                    ]);
                    ListItem::new(content)
                })
                .collect();

            let list = List::new(items)
                .block(Block::bordered().title(format!(
                    " Completed Tasks ({}) ",
                    self.tracker.entries.len()
                )))
                .highlight_style(
                    Style::default()
                        .bg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("> ")
                .highlight_spacing(HighlightSpacing::Always);

            ratatui::widgets::StatefulWidget::render(list, chunks[1], buf, &mut self.list_state);
        }

        if let Some(ref msg) = self.message {
            let message_block = Paragraph::new(msg.as_str())
                .style(Style::default().fg(self.message_color))
                .block(Block::bordered());
            message_block.render(chunks[2], buf);
        }

        let controls = match self.mode {
            InputMode::DeleteTask => Paragraph::new(vec![
                Line::from(vec![
                    Span::raw("Task number to delete: "),
                    Span::styled(&self.input, Style::default().fg(Color::Yellow)),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(vec![
                    Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                    Span::raw(" to confirm  "),
                    Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                    Span::raw(" to cancel"),
                ]),
            ]),
            InputMode::ConfirmDeleteAll => Paragraph::new(vec![Line::from(vec![
                Span::styled("Y", Style::default().fg(Color::Green).bold()),
                Span::raw(" Confirm Delete All  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Cancel  "),
            ])]),
            _ => Paragraph::new(vec![Line::from(vec![
                Span::styled("↑↓", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" Navigate  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Back to Main  "),
                Span::styled("X", Style::default().fg(Color::Red).bold()),
                Span::raw(" Delete Entry "),
                Span::styled("A", Style::default().fg(Color::Red).bold()),
                Span::raw(" Delete All"),
            ])]),
        }
        .block(Block::bordered().border_style(Style::default().fg(Color::Gray)))
        .centered();

        controls.render(chunks[3], buf);
    }

    fn render_calendar_view(&mut self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(12),
                Constraint::Length(10),
                Constraint::Length(3),
            ])
            .split(area);

        let title = Paragraph::new(format!(
            "Task Calendar - {} ",
            self.calendar_date.format("%Y, %m")
        ))
        .centered()
        .style(Style::default().fg(Color::Magenta).bold())
        .block(Block::bordered().border_style(Style::default().fg(Color::Magenta)));
        title.render(chunks[0], buf);

        // Create a 3x4 grid for 12 months
        let calendar_area = chunks[1];
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
                Constraint::Percentage(25),
            ])
            .split(calendar_area);

        // Render all 12 months
        for row_idx in 0..4 {
            let columns = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([
                    Constraint::Percentage(33),
                    Constraint::Percentage(34),
                    Constraint::Percentage(33),
                ])
                .split(rows[row_idx]);

            for col_idx in 0..3 {
                let month_num = (row_idx * 3 + col_idx + 1) as u32;

                // Create date for this month
                let month_date =
                    chrono::NaiveDate::from_ymd_opt(self.calendar_date.year(), month_num, 1)
                        .unwrap();

                let time_date = TimeDate::from_calendar_date(
                    month_date.year(),
                    time::Month::try_from(month_date.month() as u8).unwrap(),
                    1,
                )
                .unwrap();

                // Build event store for this month
                let mut event_store = CalendarEventStore::default();
                for entry in &self.tracker.entries {
                    if let Some(end) = entry.end {
                        let entry_date = end.date_naive();
                        if entry_date.year() == month_date.year() && entry_date.month() == month_num
                        {
                            let time_entry_date = TimeDate::from_calendar_date(
                                entry_date.year(),
                                time::Month::try_from(entry_date.month() as u8).unwrap(),
                                entry_date.day() as u8,
                            )
                            .unwrap();

                            // Current Day
                            event_store
                                .add(time_entry_date, Style::default().fg(Color::Cyan).bold());
                        }
                    }
                }

                // Determine if this is the current selected month
                let is_current_month = month_num == self.calendar_date.month();

                let border_style = if is_current_month {
                    Style::default().fg(Color::Magenta).bold()
                } else {
                    Style::default().fg(Color::DarkGray)
                };

                let title_style = if is_current_month {
                    Style::default().fg(Color::Magenta).bold()
                } else {
                    Style::default().fg(Color::White)
                };

                let month_name = chrono::NaiveDate::from_ymd_opt(2024, month_num, 1)
                    .unwrap()
                    .format("%b")
                    .to_string();

                let calendar = Monthly::new(time_date, event_store)
                    .block(
                        Block::bordered()
                            .title(month_name)
                            .title_style(title_style)
                            .border_style(border_style),
                    )
                    .show_surrounding(Style::default().fg(Color::DarkGray));

                calendar.render(columns[col_idx], buf);
            }
        }

        // Stats and task list for current month
        let start_of_month = self.calendar_date.with_day(1).unwrap().date_naive();

        let end_of_month = if self.calendar_date.month() == 12 {
            chrono::NaiveDate::from_ymd_opt(self.calendar_date.year() + 1, 1, 1).unwrap()
        } else {
            chrono::NaiveDate::from_ymd_opt(
                self.calendar_date.year(),
                self.calendar_date.month() + 1,
                1,
            )
            .unwrap()
        }
        .pred_opt()
        .unwrap();

        let tasks_this_month = self.tracker.task_in_range(start_of_month, end_of_month);
        let total_duration: Duration = tasks_this_month
            .iter()
            .map(|e| e.duration())
            .fold(Duration::zero(), |acc, d| acc + d);

        let hours = total_duration.num_hours();
        let minutes = total_duration.num_minutes() % 60;

        let display_day = self
            .selected_day
            .unwrap_or_else(|| Local::now().date_naive());
        let tasks_on_day = self.tracker.task_on_date(display_day);
        let duration_on_day = self.tracker.duration_on_date(display_day);
        let day_hours = duration_on_day.num_hours();
        let day_minutes = duration_on_day.num_minutes() % 60;

        let mut stats_lines = vec![
            Line::from(vec![
                Span::styled(
                    if self.selected_day.is_some() {
                        format!("{}: ", display_day.format("%B %d, %Y"))
                    } else {
                        "Today: ".to_string()
                    },
                    Style::default().fg(Color::Yellow).bold(),
                ),
                Span::styled(
                    format!("{} tasks, {}h {}m", tasks_on_day, day_hours, day_minutes),
                    Style::default().fg(Color::White),
                ),
            ]),
            Line::from(vec![
                Span::styled("This Month: ", Style::default().fg(Color::Cyan).bold()),
                Span::styled(
                    format!("{} tasks, {}h {}m", tasks_this_month.len(), hours, minutes),
                    Style::default().fg(Color::White),
                ),
            ]),
        ];

        if tasks_on_day > 0 {
            stats_lines.push(Line::from(""));
            stats_lines.push(Line::from(vec![Span::styled(
                "Tasks:",
                Style::default().fg(Color::Yellow).bold(),
            )]));

            for entry in &self.tracker.entries {
                if let Some(end) = entry.end {
                    if end.date_naive() == display_day {
                        let duration = entry.format_duration();
                        stats_lines.push(Line::from(vec![
                            Span::styled("  • ", Style::default().fg(Color::Green)),
                            Span::styled(&entry.activity, Style::default().fg(Color::White)),
                            Span::styled(
                                format!(" ({})", duration),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ]));
                    }
                }
            }
        }

        let stats = Paragraph::new(stats_lines).block(
            Block::bordered()
                .title(Title::from(" Statistics & Tasks ").alignment(Alignment::Center))
                .border_style(Style::default().fg(Color::White)),
        );

        stats.render(chunks[2], buf);

        let controls = match self.mode {
            InputMode::SelectDay => Paragraph::new(vec![
                Line::from(vec![
                    Span::raw("Enter day (1-31): "),
                    Span::styled(&self.input, Style::default().fg(Color::Yellow)),
                    Span::styled("█", Style::default().fg(Color::Yellow)),
                ]),
                Line::from(vec![
                    Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                    Span::raw(" to confirm  "),
                    Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                    Span::raw(" to cancel"),
                ]),
            ]),
            _ => Paragraph::new(vec![Line::from(vec![
                Span::styled("Arrow Keys", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" Navigate Months "),
                Span::styled("D", Style::default().fg(Color::Cyan).bold()),
                Span::raw(" Select Day  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Back to Main"),
            ])]),
        }
        .block(Block::bordered().border_style(Style::default().fg(Color::Gray)))
        .centered();

        controls.render(chunks[3], buf);
    }

    fn handle_events(&mut self) -> io::Result<()> {
        // Always drain any chat messages the background reader produced
        self.chat.poll_incoming();

        if event::poll(StdDuration::from_millis(100))? {
            if let Event::Key(key_event) = event::read()? {
                if key_event.kind == KeyEventKind::Press {
                    self.handle_key_event(key_event);
                }
            }
        }
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match self.mode {
            InputMode::Normal => self.handle_normal_mode(key_event),
            InputMode::StartCountdown => self.handle_countdown_input(key_event),
            InputMode::StopTask => self.handle_stop_task_mode(key_event),
            InputMode::DeleteTask => self.handle_delete_task_mode(key_event),
            InputMode::ConfirmDeleteAll => self.handle_confirm_delete_all_mode(key_event),
            InputMode::SelectDay => self.handle_select_day_mode(key_event),
            InputMode::ChatTyping => self.handle_chat_typing(key_event),
        }
    }

    fn handle_normal_mode(&mut self, key_event: KeyEvent) {
        match self.view {
            View::Main => match key_event.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => self.exit = true,
                KeyCode::Char('t') | KeyCode::Char('T') => {
                    self.mode = InputMode::StartCountdown;
                    self.input.clear();
                    self.message = None;
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    if self.tracker.active_entries.is_empty() {
                        self.message = Some("✗ No active tasks to stop".to_string());
                        self.message_color = Color::Red;
                    } else if self.tracker.active_entries.len() == 1 {
                        match self.tracker.stop(0) {
                            Ok(name) => {
                                self.message = Some(format!("✓ Stopped task: {}", name));
                                self.message_color = Color::Green;
                                let _ = self.tracker.save();
                            }
                            Err(e) => {
                                self.message = Some(format!("✗ Error: {}", e));
                                self.message_color = Color::Red;
                            }
                        }
                    } else {
                        self.mode = InputMode::StopTask;
                        self.input.clear();
                        self.message = None;
                    }
                }
                KeyCode::Char('c') | KeyCode::Char('C') => {
                    self.view = View::Calendar;
                    self.message = None;
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    let count = self.tracker.stop_all();
                    if count > 0 {
                        self.message = Some(format!(
                            "✓ Stopped {} task{}",
                            count,
                            if count == 1 { "" } else { "s" }
                        ));
                        self.message_color = Color::Green;
                        let _ = self.tracker.save();
                    } else {
                        self.message = Some("✗ No active tasks to stop".to_string());
                        self.message_color = Color::Red;
                    }
                }
                KeyCode::Char('h') | KeyCode::Char('H') => {
                    self.view = View::History;
                    self.message = None;
                }
                KeyCode::Char('m') | KeyCode::Char('M') => {
                    self.view = View::Chat;
                    self.message = None;
                }
                _ => {}
            },

            View::Calendar => match key_event.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.view = View::Main;
                    self.selected_day = None;
                    self.message = None;
                }
                KeyCode::Char('d') | KeyCode::Char('D') => {
                    self.mode = InputMode::SelectDay;
                    self.input.clear();
                    self.message = None;
                }
                KeyCode::ScrollLock => {
                    todo!()
                }
                KeyCode::Left => {
                    // Move to previous month
                    self.selected_day = None;
                    self.calendar_date = if self.calendar_date.month() == 1 {
                        self.calendar_date
                            .with_year(self.calendar_date.year() - 1)
                            .unwrap()
                            .with_month(12)
                            .unwrap()
                    } else {
                        self.calendar_date
                            .with_month(self.calendar_date.month() - 1)
                            .unwrap()
                    };
                }
                KeyCode::Right => {
                    // Move to next month
                    self.selected_day = None;
                    self.calendar_date = if self.calendar_date.month() == 12 {
                        self.calendar_date
                            .with_year(self.calendar_date.year() + 1)
                            .unwrap()
                            .with_month(1)
                            .unwrap()
                    } else {
                        self.calendar_date
                            .with_month(self.calendar_date.month() + 1)
                            .unwrap()
                    };
                }
                KeyCode::Up => {
                    // Move up 3 months (one row up in the grid)
                    self.selected_day = None;
                    let current_month = self.calendar_date.month() as i32;
                    let new_month = current_month - 3;

                    if new_month <= 0 {
                        // Wrap to previous year
                        self.calendar_date = self
                            .calendar_date
                            .with_year(self.calendar_date.year() - 1)
                            .unwrap()
                            .with_month((new_month + 12) as u32)
                            .unwrap();
                    } else {
                        self.calendar_date =
                            self.calendar_date.with_month(new_month as u32).unwrap();
                    }
                }
                KeyCode::Down => {
                    // Move down 3 months (one row down in the grid)
                    self.selected_day = None;
                    let current_month = self.calendar_date.month() as i32;
                    let new_month = current_month + 3;

                    if new_month > 12 {
                        // Wrap to next year
                        self.calendar_date = self
                            .calendar_date
                            .with_year(self.calendar_date.year() + 1)
                            .unwrap()
                            .with_month((new_month - 12) as u32)
                            .unwrap();
                    } else {
                        self.calendar_date =
                            self.calendar_date.with_month(new_month as u32).unwrap();
                    }
                }
                _ => {}
            },

            View::History => match key_event.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.view = View::Main;
                    self.message = None;
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    if self.tracker.entries.is_empty() {
                        self.message = Some("✗ No task to delete".to_string());
                        self.message_color = Color::Red;
                    } else {
                        self.mode = InputMode::DeleteTask;
                        self.input.clear();
                        self.message = None;
                    }
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    if self.tracker.entries.is_empty() {
                        self.message = Some(format!("No tasks to Delete"));
                        self.message_color = Color::Red;
                    } else {
                        self.mode = InputMode::ConfirmDeleteAll;
                        self.input.clear();
                        self.message = None;
                    }
                }
                KeyCode::Up => {
                    if !self.tracker.entries.is_empty() {
                        let i = self.list_state.selected().unwrap_or(0);
                        if i > 0 {
                            self.list_state.select(Some(i - 1));
                        }
                    }
                }
                KeyCode::Down => {
                    if !self.tracker.entries.is_empty() {
                        let i = self.list_state.selected().unwrap_or(0);
                        if i < self.tracker.entries.len() - 1 {
                            self.list_state.select(Some(i + 1));
                        }
                    }
                }
                _ => {}
            },

            View::Chat => match key_event.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.view = View::Main;
                }
                KeyCode::Char('i') | KeyCode::Char('I') => {
                    self.mode = InputMode::ChatTyping;
                }
                _ => {}
            },
        }
    }

    fn handle_stop_task_mode(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    if let Ok(num) = self.input.parse::<usize>() {
                        if num > 0 && num <= self.tracker.active_entries.len() {
                            match self.tracker.stop(num - 1) {
                                Ok(name) => {
                                    self.message = Some(format!("✓ Stopped task: {}", name));
                                    self.message_color = Color::Green;
                                    let _ = self.tracker.save();
                                }
                                Err(e) => {
                                    self.message = Some(format!("✗ Error: {}", e));
                                    self.message_color = Color::Red;
                                }
                            }
                        } else {
                            self.message = Some(format!(
                                "✗ Invalid task number (1-{})",
                                self.tracker.active_entries.len()
                            ));
                            self.message_color = Color::Red;
                        }
                    } else {
                        self.message = Some("✗ Please enter a valid number".to_string());
                        self.message_color = Color::Red;
                    }
                }
                self.mode = InputMode::Normal;
                self.input.clear();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.input.push(c);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.input.clear();
                self.message = Some("Cancelled".to_string());
                self.message_color = Color::Yellow;
            }
            _ => {}
        }
    }

    fn handle_delete_task_mode(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    if let Ok(num) = self.input.parse::<usize>() {
                        if num > 0 && num <= self.tracker.entries.len() {
                            let index = num - 1;
                            match self.tracker.delete_task(index) {
                                Ok(name) => {
                                    self.message = Some(format!("✓ Deleted task: {}", name));
                                    self.message_color = Color::Green;
                                    let _ = self.tracker.save();
                                }
                                Err(e) => {
                                    self.message = Some(format!("✗ Error: {}", e));
                                    self.message_color = Color::Red;
                                }
                            }
                        } else {
                            self.message = Some(format!(
                                "✗ Invalid task number (1-{})",
                                self.tracker.entries.len()
                            ));
                            self.message_color = Color::Red;
                        }
                    } else {
                        self.message = Some("✗ Please enter a valid number".to_string());
                        self.message_color = Color::Red;
                    }
                }
                self.mode = InputMode::Normal;
                self.input.clear();
            }
            KeyCode::Char('a') | KeyCode::Char('A') => {
                if !self.input.is_empty() {
                    if let Ok(num) = self.input.parse::<usize>() {}
                } else {
                    self.message = Some("✗ No active tasks to stop".to_string());
                    self.message_color = Color::Red;
                }
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                self.input.push(c);
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.input.clear();
                self.message = Some("Cancelled".to_string());
                self.message_color = Color::Yellow;
            }
            _ => {}
        }
    }

    fn handle_confirm_delete_all_mode(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                let count = self.tracker.delete_all();
                self.message = Some(format!(
                    "Deleted {} task{}",
                    count,
                    if count == 1 { "" } else { "s" }
                ));
                self.message_color = Color::Green;
                let _ = self.tracker.save();
                self.mode = InputMode::Normal;
            }
            KeyCode::Esc => {
                self.message = Some("Cancelled".to_string());
                self.message_color = Color::Yellow;
                self.mode = InputMode::Normal;
            }
            _ => {}
        }
    }

    fn handle_select_day_mode(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    if let Ok(day) = self.input.parse::<u32>() {
                        if let Some(date) = chrono::NaiveDate::from_ymd_opt(
                            self.calendar_date.year(),
                            self.calendar_date.month(),
                            day,
                        ) {
                            self.selected_day = Some(date);
                            self.message =
                                Some(format!("Viewing tasks for {}", date.format("%B %d, %Y")));
                            self.message_color = Color::Cyan;
                        } else {
                            self.message = Some("✗ Invalid day for this month".to_string());
                            self.message_color = Color::Red;
                        }
                    } else {
                        self.message = Some("✗ Please enter a valid day number".to_string());
                        self.message_color = Color::Red;
                    }
                }
                self.mode = InputMode::Normal;
                self.input.clear();
            }
            KeyCode::Char(c) if c.is_ascii_digit() => {
                if self.input.len() < 2 {
                    self.input.push(c);
                }
            }
            KeyCode::Backspace => {
                self.input.pop();
            }
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.input.clear();
                self.selected_day = None;
                self.message = Some("Cancelled".to_string());
                self.message_color = Color::Yellow;
            }
            _ => {}
        }
    }

    // ──────────────────────────────────────────────────────────────
    // CHAT VIEW
    // ──────────────────────────────────────────────────────────────

    fn render_chat_view(&mut self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),  // title
                Constraint::Min(5),     // message log
                Constraint::Length(3),  // input bar
                Constraint::Length(3),  // controls hint
            ])
            .split(area);

        // Title bar
        let conn_label = match &self.chat.connection_state {
            ChatConnectionState::Disconnected => " [disconnected] ".to_string(),
            ChatConnectionState::Connected => " [connected – not registered] ".to_string(),
            ChatConnectionState::Registered(name) => format!(" [{}] ", name),
        };
        let title = Paragraph::new(format!("💬 Secure Chat  {}", conn_label))
            .centered()
            .style(Style::default().fg(Color::Blue).bold())
            .block(Block::bordered().border_style(Style::default().fg(Color::Blue)));
        title.render(chunks[0], buf);

        // Message log
        let visible_height = chunks[1].height.saturating_sub(2) as usize;
        let msgs = &self.chat.messages;
        let start = if msgs.len() > visible_height {
            msgs.len() - visible_height
        } else {
            0
        };
        let log_lines: Vec<Line> = msgs[start..]
            .iter()
            .map(|(text, color)| {
                Line::from(Span::styled(text.clone(), Style::default().fg(*color)))
            })
            .collect();

        let log = Paragraph::new(log_lines)
            .block(Block::bordered().title(" Messages ").border_style(
                Style::default().fg(Color::White),
            ))
            .wrap(Wrap { trim: false });
        log.render(chunks[1], buf);

        // Input bar
        let input_content = match &self.mode {
            InputMode::ChatTyping => {
                Line::from(vec![
                    Span::styled("> ", Style::default().fg(Color::Blue).bold()),
                    Span::styled(&self.chat.input, Style::default().fg(Color::White)),
                    Span::styled("█", Style::default().fg(Color::Blue)),
                ])
            }
            _ => {
                Line::from(vec![
                    Span::styled(
                        "Press I to type a command",
                        Style::default().fg(Color::DarkGray).italic(),
                    ),
                ])
            }
        };
        let input_block = Paragraph::new(input_content)
            .block(Block::bordered().border_style(Style::default().fg(Color::Blue)));
        input_block.render(chunks[2], buf);

        // Controls
        let hint = if matches!(self.mode, InputMode::ChatTyping) {
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                Span::raw(" Send  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Cancel input  "),
                Span::styled(
                    "  Commands: REGISTER <name>  MSG <user> <text>  LIST  QUIT",
                    Style::default().fg(Color::DarkGray),
                ),
            ])
        } else {
            Line::from(vec![
                Span::styled("I", Style::default().fg(Color::Blue).bold()),
                Span::raw(" Type  "),
                Span::styled("Esc/Q", Style::default().fg(Color::Red).bold()),
                Span::raw(" Back to Main  "),
                Span::styled(
                    "  [Messages are AES-128-CTR encrypted]",
                    Style::default().fg(Color::DarkGray).italic(),
                ),
            ])
        };
        let controls = Paragraph::new(hint)
            .block(Block::bordered().border_style(Style::default().fg(Color::Gray)))
            .centered();
        controls.render(chunks[3], buf);
    }

    fn handle_chat_typing(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Esc => {
                self.mode = InputMode::Normal;
                self.chat.input.clear();
            }
            KeyCode::Backspace => {
                self.chat.input.pop();
            }
            KeyCode::Enter => {
                let raw = self.chat.input.trim().to_string();
                self.chat.input.clear();
                self.mode = InputMode::Normal;
                if raw.is_empty() {
                    return;
                }
                self.dispatch_chat_command(&raw);
            }
            KeyCode::Char(c) => {
                self.chat.input.push(c);
            }
            _ => {}
        }
    }

    fn dispatch_chat_command(&mut self, raw: &str) {
        let parts: Vec<&str> = raw.splitn(3, ' ').collect();
        let cmd = parts[0].to_uppercase();

        match cmd.as_str() {
            // ── CONNECT (not a relay protocol command, but a TUI helper) ──────
            "CONNECT" => {
                let host = parts.get(1).copied().unwrap_or("167.172.239.107");
                let port: u16 = parts
                    .get(2)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(5000);
                match self.chat.connect(host, port) {
                    Ok(_) => {
                        self.chat
                            .push_msg("[system] Connected to relay server.", Color::Yellow);
                    }
                    Err(e) => {
                        self.chat.push_msg(format!("[system] {}", e), Color::Red);
                    }
                }
            }
            // ── REGISTER ──────────────────────────────────────────────────────
            "REGISTER" => {
                if self.chat.connection_state == ChatConnectionState::Disconnected {
                    // Auto-connect first
                    match self.chat.connect("167.172.239.107", 5000) {
                        Ok(_) => {}
                        Err(e) => {
                            self.chat.push_msg(format!("[system] {}", e), Color::Red);
                            return;
                        }
                    }
                }
                self.chat.send_raw(raw);
                self.chat
                    .push_msg(format!("[you] {}", raw), Color::DarkGray);
            }
            // ── LIST ──────────────────────────────────────────────────────────
            "LIST" => {
                self.chat.send_raw("LIST");
                self.chat
                    .push_msg("[you] LIST", Color::DarkGray);
            }
            // ── MSG ───────────────────────────────────────────────────────────
            "MSG" => {
                if parts.len() < 3 {
                    self.chat
                        .push_msg("[error] Usage: MSG <recipient> <message>", Color::Red);
                    return;
                }
                let recipient = parts[1];
                let plaintext = parts[2];

                // Show locally (already decrypted)
                let me = match &self.chat.connection_state {
                    ChatConnectionState::Registered(n) => n.clone(),
                    _ => "me".to_string(),
                };
                self.chat
                    .push_msg(format!("{} → {}: {}", me, recipient, plaintext), Color::Cyan);

                self.chat.send_msg(recipient, plaintext);
            }
            // ── QUIT ─────────────────────────────────────────────────────────
            "QUIT" => {
                self.chat.send_raw("QUIT");
                self.chat
                    .push_msg("[system] Disconnected.", Color::Yellow);
                self.chat.connection_state = ChatConnectionState::Disconnected;
                self.chat.tx = None;
                self.chat.rx = None;
            }
            _ => {
                self.chat
                    .push_msg(format!("[error] Unknown command: {}", raw), Color::Red);
                self.chat.push_msg(
                    "[help] Commands: REGISTER <name>  MSG <user> <text>  LIST  QUIT",
                    Color::DarkGray,
                );
            }
        }
    }
}
