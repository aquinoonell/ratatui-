use base64::{engine::general_purpose::STANDARD as B64, Engine};
use chrono::{DateTime, Datelike, Duration, Local};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};

mod crypto;
use crypto::{Handshake, Identity, KnownUsers, SessionKeys};

/// Default relay server - point this at your own VM
const RELAY_HOST: &str = "192.168.1.160";
const RELAY_PORT: u16 = 5000;

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

// ─────────────────────────────────────────────────────────────────────────────
// CHAT MODULE  - End-to-end encrypted relay chat
//
// Protocol phases:
//   1. REGISTER  → relay learns username; client sends its Ed25519 public key
//   2. SECURE    → initiates X25519 ECDH handshake with peer via HELLO messages
//   3. MSG       → ChaCha20-Poly1305 AEAD + per-message symmetric ratchet
//
// The relay server sees only:
//   REGISTER, LIST, QUIT commands (plaintext metadata)
//   MSG <recipient> <base64_ciphertext>  (never plaintext content)
// ─────────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum ChatConnectionState {
    Disconnected,
    Connected,
    Registered(String), // holds our username
}

/// State of the secure session with the current peer.
enum SecureSessionState {
    /// No secure session - messages sent as INSECURE plain warning
    None,
    /// We sent our HELLO, waiting for peer's HELLO
    AwaitingHello {
        handshake: Handshake,
        peer_username: String,
        /// Peer's Ed25519 public key (base64) - retrieved before handshake
        peer_pubkey_b64: String,
    },
    /// Both HELLOs exchanged - session keys derived
    Established(SessionKeys),
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
    /// Our loaded Ed25519 identity (loaded on REGISTER)
    identity: Option<Identity>,
    /// TOFU store of known peer public keys
    known_users: KnownUsers,
    /// Current secure session state
    session: SecureSessionState,
}

impl ChatState {
    fn new() -> Self {
        ChatState {
            connection_state: ChatConnectionState::Disconnected,
            messages: Vec::new(),
            input: String::new(),
            tx: None,
            rx: None,
            identity: None,
            known_users: KnownUsers::load(),
            session: SecureSessionState::None,
        }
    }

    fn push_msg(&mut self, text: impl Into<String>, color: Color) {
        let mut msgs = std::mem::take(&mut self.messages);
        msgs.push((text.into(), color));
        if msgs.len() > 200 {
            msgs.drain(0..msgs.len() - 200);
        }
        self.messages = msgs;
    }

    fn my_username(&self) -> String {
        match &self.connection_state {
            ChatConnectionState::Registered(n) => n.clone(),
            _ => "me".to_string(),
        }
    }

    /// Return the peer username if a secure session is established.
    fn active_peer(&self) -> Option<&str> {
        if let SecureSessionState::Established(ref keys) = self.session {
            Some(&keys.peer_username)
        } else {
            None
        }
    }

    /// TCP connect + spawn reader/writer threads.
    fn connect(&mut self, host: &str, port: u16) -> Result<(), String> {
        let addr = format!("{}:{}", host, port);
        let stream = TcpStream::connect(&addr)
            .map_err(|e| format!("TCP connect failed: {}", e))?;
        let stream_write = stream.try_clone()
            .map_err(|e| format!("Stream clone failed: {}", e))?;

        let (tx_send, rx_send): (Sender<String>, Receiver<String>) = mpsc::channel();
        let (tx_recv, rx_recv): (Sender<String>, Receiver<String>) = mpsc::channel();

        std::thread::spawn(move || {
            let mut writer = stream_write;
            for line in rx_send {
                let data = format!("{}\n", line);
                if writer.write_all(data.as_bytes()).is_err() {
                    break;
                }
            }
        });

        std::thread::spawn(move || {
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                match line {
                    Ok(l) => { if tx_recv.send(l).is_err() { break; } }
                    Err(_) => break,
                }
            }
        });

        self.tx = Some(tx_send);
        self.rx = Some(rx_recv);
        self.connection_state = ChatConnectionState::Connected;
        Ok(())
    }

    fn send_raw(&self, line: &str) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(line.to_string());
        }
    }

    /// Drain incoming lines from reader thread and process them.
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
        // ── Server control messages ────────────────────────────────────────
        if let Some(rest) = line.strip_prefix("INFO ") {
            if let Some(name) = rest.strip_prefix("Registered as ") {
                self.connection_state =
                    ChatConnectionState::Registered(name.trim().to_string());
            }
            self.push_msg(format!("[server] {}", rest), Color::DarkGray);
            return;
        }
        if let Some(rest) = line.strip_prefix("ERROR ") {
            self.push_msg(format!("[error] {}", rest), Color::Red);
            return;
        }
        if let Some(rest) = line.strip_prefix("USERLIST ") {
            self.push_msg(format!("[online] {}", rest), Color::Cyan);
            return;
        }

        // ── Messages routed by relay: FROM <sender> <payload> ─────────────
        if let Some(rest) = line.strip_prefix("FROM ") {
            let mut parts = rest.splitn(3, ' ');
            let sender    = parts.next().unwrap_or("?").to_string();
            let msg_type  = parts.next().unwrap_or("").to_string();
            let payload   = parts.next().unwrap_or("").to_string();

            match msg_type.as_str() {
                // ── PUBKEY response: peer is announcing their identity key ──
                "PUBKEY" => {
                    self.handle_incoming_pubkey(&sender, &payload);
                }
                // ── HELLO: peer's ephemeral key + signature ────────────────
                "HELLO" => {
                    self.handle_incoming_hello(&sender, &payload);
                }
                // ── Encrypted message payload ──────────────────────────────
                "ENC" => {
                    self.handle_incoming_enc(&sender, &payload);
                }
                // ── Anything else: show raw (covers plain text from old clients) ─
                _ => {
                    self.push_msg(
                        format!("[plain/{}] {}: {} {}", msg_type, sender, msg_type, payload),
                        Color::Yellow,
                    );
                }
            }
            return;
        }

        // Fallback
        self.push_msg(line.to_string(), Color::White);
    }

    /// Peer sent us their Ed25519 public key. Run TOFU check, then if we were
    /// waiting for it, fill in the peer_pubkey_b64 so the session can be derived.
    fn handle_incoming_pubkey(&mut self, sender: &str, pubkey_b64: &str) {
        match self.known_users.check_and_update(sender, pubkey_b64) {
            Ok(true) => {
                self.push_msg(
                    format!("First contact with {} - key trusted and stored (TOFU).", sender),
                    Color::Yellow,
                );
            }
            Ok(false) => {
                self.push_msg(
                    format!("{} identity key verified (matches stored).", sender),
                    Color::Green,
                );
            }
            Err(warning) => {
                self.push_msg(warning, Color::Red);
                // Do NOT proceed - reset any pending session
                self.session = SecureSessionState::None;
                return;
            }
        }

        // If we are in AwaitingHello and the peer_pubkey_b64 was empty
        // (we didn't have their key before SECURE), fill it in now.
        if let SecureSessionState::AwaitingHello { peer_pubkey_b64, peer_username, .. } =
            &mut self.session
        {
            if peer_username == sender && peer_pubkey_b64.is_empty() {
                *peer_pubkey_b64 = pubkey_b64.to_string();
                self.push_msg(
                    format!("[handshake] Received {}'s public key. Awaiting their HELLO...", sender),
                    Color::DarkGray,
                );
            }
        }
    }

    /// Peer sent us their HELLO (eph pub + Ed25519 signature).
    /// payload = "<eph_pub_b64> <sig_b64>"
    fn handle_incoming_hello(&mut self, sender: &str, payload: &str) {
        let mut p = payload.splitn(2, ' ');
        let peer_eph_b64 = p.next().unwrap_or("").to_string();
        let peer_sig_b64 = p.next().unwrap_or("").to_string();

        // We might be the responder (no pending AwaitingHello) or the initiator.
        let (my_handshake, peer_pubkey_b64, i_am_initiator) =
            match std::mem::replace(&mut self.session, SecureSessionState::None) {
                // We are the initiator - we already sent our HELLO, now receiving theirs
                SecureSessionState::AwaitingHello {
                    handshake,
                    peer_username,
                    peer_pubkey_b64,
                } if peer_username == sender => {
                    if peer_pubkey_b64.is_empty() {
                        // We don't have their key yet - this is a race condition.
                        // Store the HELLO payload temporarily and wait for PUBKEY.
                        // For simplicity: re-queue and show an error.
                        self.push_msg(
                            format!("[handshake] Received HELLO from {} before their PUBKEY - re-run SECURE.", sender),
                            Color::Red,
                        );
                        self.session = SecureSessionState::AwaitingHello {
                            handshake,
                            peer_username,
                            peer_pubkey_b64,
                        };
                        return;
                    }
                    (handshake, peer_pubkey_b64, true)
                }
                // We are the responder - received HELLO first, respond with PUBKEY + HELLO
                _ => {
                    let peer_pub = match self.known_users.users().get(sender) {
                        Some(k) => k.clone(),
                        None => {
                            // We don't know their key yet but they sent HELLO.
                            // They should have sent PUBKEY first - ask them.
                            self.push_msg(
                                format!(
                                    "[error] Got HELLO from {} but no public key stored. They should re-run SECURE.",
                                    sender
                                ),
                                Color::Red,
                            );
                            return;
                        }
                    };
                    let new_hs = Handshake::new();
                    // Send our public key first so they can TOFU-store us
                    if let Some(id) = &self.identity {
                        let my_pubkey = id.public_key_b64();
                        self.send_raw(&format!("MSG {} PUBKEY {}", sender, my_pubkey));
                        // Now send our HELLO
                        let hello = new_hs.hello_line(id);
                        let hello_parts: Vec<&str> = hello.splitn(4, ' ').collect();
                        if hello_parts.len() == 4 {
                            self.send_raw(&format!(
                                "MSG {} HELLO {} {}",
                                sender, hello_parts[2], hello_parts[3]
                            ));
                        }
                    }
                    (new_hs, peer_pub, false)
                }
            };

        // Derive session keys
        match my_handshake.derive_session(
            sender,
            &peer_pubkey_b64,
            &peer_eph_b64,
            &peer_sig_b64,
            i_am_initiator,
        ) {
            Ok(session_keys) => {
                self.push_msg(
                    format!(
                        "Secure session established with {} (ChaCha20-Poly1305 + ratchet)",
                        sender
                    ),
                    Color::Green,
                );
                self.session = SecureSessionState::Established(session_keys);
            }
            Err(e) => {
                self.push_msg(format!("[handshake error] {}", e), Color::Red);
                self.session = SecureSessionState::None;
            }
        }
    }

    /// Decrypt an incoming ENC message using the ratchet.
    fn handle_incoming_enc(&mut self, sender: &str, wire_b64: &str) {
        if let SecureSessionState::Established(ref mut keys) = self.session {
            match keys.decrypt(wire_b64) {
                Ok(plaintext) => {
                    self.push_msg(format!("{}: {}", sender, plaintext), Color::Green);
                }
                Err(e) => {
                    self.push_msg(format!("[decrypt error] {}", e), Color::Red);
                }
            }
        } else {
            self.push_msg(
                format!("[error] Got encrypted message from {} but no secure session active - run SECURE {} first", sender, sender),
                Color::Red,
            );
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
                    self.mode = InputMode::ChatTyping;
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
                Constraint::Length(3),  // title + session status
                Constraint::Min(5),     // message log
                Constraint::Length(3),  // input bar
                Constraint::Length(3),  // controls hint
            ])
            .split(area);

        // Title bar - two lines: username row + session/peer row
        let conn_label = match &self.chat.connection_state {
            ChatConnectionState::Disconnected => "[disconnected]".to_string(),
            ChatConnectionState::Connected => "[connected - not registered]".to_string(),
            ChatConnectionState::Registered(name) => format!("[{}]", name),
        };
        let session_label = match self.chat.active_peer() {
            Some(peer) => format!("chatting with: {}", peer),
            None => match &self.chat.session {
                SecureSessionState::AwaitingHello { peer_username, .. } =>
                    format!("handshake with {} in progress...", peer_username),
                _ => "type: REGISTER <name> then SECURE <peer>".to_string(),
            },
        };
        let title = Paragraph::new(vec![
            Line::from(Span::styled(
                format!("Secure Chat  {}", conn_label),
                Style::default().fg(Color::Blue).bold(),
            )),
            Line::from(Span::styled(
                session_label,
                Style::default().fg(Color::Cyan),
            )),
        ])
        .centered()
        .block(Block::bordered().border_style(Style::default().fg(Color::Blue)));
        title.render(chunks[0], buf);

        // Message log - account for line wrapping so newest messages are always visible
        let inner_width = chunks[1].width.saturating_sub(2) as usize; // subtract borders
        let visible_height = chunks[1].height.saturating_sub(2) as usize;

        // Count how many rendered rows each message takes (word-wrap simulation)
        let msgs = &self.chat.messages;
        let row_counts: Vec<usize> = msgs.iter().map(|(text, _)| {
            if inner_width == 0 { 1 } else {
                text.len().saturating_sub(1) / inner_width + 1
            }
        }).collect();

        // Walk from the end, accumulating rows until we fill visible_height
        let total_rows: usize = row_counts.iter().sum();
        let mut rows_to_show = visible_height.min(total_rows);
        let mut start_idx = msgs.len();
        let mut accumulated = 0;
        while start_idx > 0 && accumulated < rows_to_show {
            start_idx -= 1;
            accumulated += row_counts[start_idx];
        }
        // If we overshot, move start forward one
        if accumulated > rows_to_show && start_idx + 1 < msgs.len() {
            start_idx += 1;
        }

        let log_lines: Vec<Line> = msgs[start_idx..]
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
                    Span::styled("_", Style::default().fg(Color::Blue)),
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
            let cmd_hint = if let Some(peer) = self.chat.active_peer() {
                format!("  Just type to send to {}  |  Commands: SECURE <peer>  LIST  QUIT", peer)
            } else {
                "  Commands: REGISTER <name>  SECURE <peer>  LIST  QUIT".to_string()
            };
            Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                Span::raw(" Send  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Cancel  "),
                Span::styled(cmd_hint, Style::default().fg(Color::DarkGray)),
            ])
        } else {
            Line::from(vec![
                Span::styled("I", Style::default().fg(Color::Blue).bold()),
                Span::raw(" Type  "),
                Span::styled("Esc/Q", Style::default().fg(Color::Red).bold()),
                Span::raw(" Back to Main  "),
                Span::styled(
                    "  [ChaCha20-Poly1305 + X25519 + Ed25519 + Ratchet]",
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
                // Stay in ChatTyping - no need to press I before each command
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

        // ── Quick-send shortcut ───────────────────────────────────────────────
        // If the user types text that isn't a known command AND there's an active
        // secure session, treat the whole input as the message body to the peer.
        let known_commands = ["CONNECT", "REGISTER", "PUBKEY", "SECURE", "LIST", "MSG", "QUIT"];
        if !known_commands.contains(&cmd.as_str()) {
            let peer_opt = self.chat.active_peer().map(|s| s.to_string());
            if let Some(peer) = peer_opt {
                let plaintext = raw.to_string();
                if let SecureSessionState::Established(ref mut keys) = self.chat.session {
                    match keys.encrypt(&plaintext) {
                        Ok(wire) => {
                            self.chat.send_raw(&format!("MSG {} ENC {}", peer, wire));
                            let me = self.chat.my_username();
                            self.chat.push_msg(
                                format!("{}: {}", me, plaintext),
                                Color::Cyan,
                            );
                        }
                        Err(e) => {
                            self.chat.push_msg(format!("[encrypt error] {}", e), Color::Red);
                        }
                    }
                }
                return;
            }
        }

        match cmd.as_str() {
            // ── CONNECT ───────────────────────────────────────────────────────
            "CONNECT" => {
                let host = parts.get(1).copied().unwrap_or(RELAY_HOST);
                let port: u16 = parts
                    .get(2)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(RELAY_PORT);
                match self.chat.connect(host, port) {
                    Ok(_) => self.chat.push_msg("[system] Connected to relay server.", Color::Yellow),
                    Err(e) => self.chat.push_msg(format!("[system] {}", e), Color::Red),
                }
            }

            // ── REGISTER ──────────────────────────────────────────────────────
            // Loads/generates Ed25519 identity, connects, registers with relay,
            // then broadcasts our public key so peers can store it for TOFU.
            "REGISTER" => {
                let username = match parts.get(1).copied() {
                    Some(u) => u.to_string(),
                    None => {
                        self.chat.push_msg("[error] Usage: REGISTER <username>", Color::Red);
                        return;
                    }
                };

                // Load or generate identity keypair
                match Identity::load_or_create(&username) {
                    Ok(identity) => {
                        let pubkey = identity.public_key_b64();
                        self.chat.push_msg(
                            format!("Identity key loaded. Fingerprint: {}...", &pubkey[..pubkey.len().min(16)]),
                            Color::Yellow,
                        );
                        self.chat.identity = Some(identity);
                    }
                    Err(e) => {
                        self.chat.push_msg(format!("[error] Key load failed: {}", e), Color::Red);
                        return;
                    }
                }

                // Auto-connect if needed
                if self.chat.connection_state == ChatConnectionState::Disconnected {
                    match self.chat.connect(RELAY_HOST, RELAY_PORT) {
                        Ok(_) => {}
                        Err(e) => {
                            self.chat.push_msg(format!("[system] {}", e), Color::Red);
                            return;
                        }
                    }
                }

                // Register username with relay
                self.chat.send_raw(&format!("REGISTER {}", username));

                if let Some(id) = &self.chat.identity {
                    let pubkey = id.public_key_b64();
                    self.chat.push_msg(
                        format!("[system] Registered. Run: SECURE <peer> to start an encrypted session."),
                        Color::DarkGray,
                    );
                    self.chat.push_msg(
                        format!("[system] Your key fingerprint: {}...", &pubkey[..pubkey.len().min(24)]),
                        Color::DarkGray,
                    );
                }
            }

            // ── PUBKEY - broadcast our public key to a specific peer ──────────
            "PUBKEY" => {
                let target = match parts.get(1).copied() {
                    Some(t) => t,
                    None => {
                        self.chat.push_msg("[error] Usage: PUBKEY <username>", Color::Red);
                        return;
                    }
                };
                if let Some(id) = &self.chat.identity {
                    let pubkey = id.public_key_b64();
                    self.chat.send_raw(&format!("MSG {} PUBKEY {}", target, pubkey));
                    self.chat.push_msg(
                        format!("[system] Public key sent to {}.", target),
                        Color::DarkGray,
                    );
                } else {
                    self.chat.push_msg("[error] Not registered yet.", Color::Red);
                }
            }

            // ── SECURE - initiate authenticated key exchange with a peer ──────
            // Flow (automatic, no manual PUBKEY step required):
            //   1. Send our Ed25519 public key to peer (PUBKEY message)
            //   2. Send our X25519 ephemeral HELLO to peer (HELLO message)
            //   3. Peer receives HELLO, sends their PUBKEY + their HELLO back
            //   4. We receive their PUBKEY (TOFU check) then derive session keys
            "SECURE" => {
                let peer = match parts.get(1).copied() {
                    Some(p) => p.to_string(),
                    None => {
                        self.chat.push_msg("[error] Usage: SECURE <username>", Color::Red);
                        return;
                    }
                };

                if self.chat.identity.is_none() {
                    self.chat.push_msg("[error] REGISTER first.", Color::Red);
                    return;
                }

                // Step 1: Send our public key to peer so they can TOFU-store it
                let pubkey = self.chat.identity.as_ref().unwrap().public_key_b64();
                self.chat.send_raw(&format!("MSG {} PUBKEY {}", peer, pubkey));

                // Step 2: Generate ephemeral X25519 keypair and send HELLO
                let identity = self.chat.identity.as_ref().unwrap();
                let handshake = Handshake::new();
                let hello = handshake.hello_line(identity);
                // hello = "HELLO <username> <eph_pub_b64> <sig_b64>"
                let hello_parts: Vec<&str> = hello.splitn(4, ' ').collect();
                if hello_parts.len() == 4 {
                    self.chat.send_raw(&format!(
                        "MSG {} HELLO {} {}",
                        peer, hello_parts[2], hello_parts[3]
                    ));
                }

                // Check if we already have their key stored (returning user)
                let ku = KnownUsers::load();
                let peer_pubkey_b64 = ku.users().get(&peer).cloned().unwrap_or_default();

                if peer_pubkey_b64.is_empty() {
                    self.chat.push_msg(
                        format!("[handshake] Sent PUBKEY + HELLO to {}. Waiting for their PUBKEY...", peer),
                        Color::Yellow,
                    );
                } else {
                    self.chat.push_msg(
                        format!("[handshake] Sent PUBKEY + HELLO to {} (key already known). Awaiting their HELLO...", peer),
                        Color::Yellow,
                    );
                }

                self.chat.session = SecureSessionState::AwaitingHello {
                    handshake,
                    peer_username: peer.clone(),
                    peer_pubkey_b64,
                };
            }

            // ── LIST ──────────────────────────────────────────────────────────
            "LIST" => {
                self.chat.send_raw("LIST");
                self.chat.push_msg("[you] LIST", Color::DarkGray);
            }

            // ── MSG - send encrypted message (requires SECURE session) ────────
            "MSG" => {
                if parts.len() < 3 {
                    self.chat.push_msg("[error] Usage: MSG <recipient> <message>", Color::Red);
                    return;
                }
                let recipient = parts[1].to_string();
                let plaintext = parts[2];

                match &mut self.chat.session {
                    SecureSessionState::Established(ref mut keys)
                        if keys.peer_username == recipient =>
                    {
                        match keys.encrypt(plaintext) {
                            Ok(wire) => {
                                self.chat.send_raw(&format!("MSG {} ENC {}", recipient, wire));
                                let me = self.chat.my_username();
                                self.chat.push_msg(
                                    format!("{} → {}: {}", me, recipient, plaintext),
                                    Color::Cyan,
                                );
                            }
                            Err(e) => {
                                self.chat.push_msg(format!("[encrypt error] {}", e), Color::Red);
                            }
                        }
                    }
                    _ => {
                        self.chat.push_msg(
                            format!(
                                "[error] No secure session with {}. Run: SECURE {}",
                                recipient, recipient
                            ),
                            Color::Red,
                        );
                    }
                }
            }

            // ── QUIT ─────────────────────────────────────────────────────────
            "QUIT" => {
                self.chat.send_raw("QUIT");
                self.chat.push_msg("[system] Disconnected.", Color::Yellow);
                self.chat.connection_state = ChatConnectionState::Disconnected;
                self.chat.session = SecureSessionState::None;
                self.chat.tx = None;
                self.chat.rx = None;
            }

            _ => {
                self.chat.push_msg(format!("[error] Unknown command: {}", raw), Color::Red);
                self.chat.push_msg(
                    "[help] REGISTER <name>  SECURE <peer>  MSG <peer> <text>  LIST  QUIT",
                    Color::DarkGray,
                );
            }
        }
    }
}
