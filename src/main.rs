use chrono::{DateTime, Duration, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Widget, Wrap},
    DefaultTerminal, Frame,
};
use serde::{Deserialize, Serialize};
use std::io;
use std::path::PathBuf;
use std::time::Duration as StdDuration;
use std::{fmt::format, fs};

fn main() -> io::Result<()> {
    let mut terminal = ratatui::init();
    let app_result = App::new().run(&mut terminal);
    ratatui::restore();
    app_result
}

#[derive(Serialize, Deserialize, Clone)]
struct TimeEntry {
    activity: String,
    start: DateTime<Local>,
    end: Option<DateTime<Local>>,
}

impl TimeEntry {
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
    // Changed the previous Option<TimeEntry for Vec<TimeEntry> in order to get multiple tasks.
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
            end: None,
        });
        Ok(())
    }

    fn stop(&mut self, index: usize) -> Result<String, String> {
        if index >= self.active_entries.len() {
            return Err("Invalid task index".to_string());
        }

        let mut entry = self.active_entries.remove(index);
        entry.end = Some(Local::now());
        let name = entry.activity.clone();
        self.entries.push(entry);
        Ok(name)
    }

    fn stop_all(&mut self) -> usize {
        let count = self.active_entries.len();
        let now = Local::now();

        for mut entry in self.active_entries.drain(..) {
            entry.end = Some(now);
            self.entries.push(entry);
        }

        count
    }
}

enum InputMode {
    Normal,
    StartTask,
    StopTask,
}

enum View {
    Main,
    History,
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
        }
    }

    pub fn run(&mut self, terminal: &mut DefaultTerminal) -> io::Result<()> {
        while !self.exit {
            terminal.draw(|frame| self.draw(frame))?;
            self.handle_events()?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let area = frame.area();

        match self.view {
            View::Main => self.render_main_view(area, frame.buffer_mut()),
            View::History => self.render_history_view(area, frame.buffer_mut()),
        }
    }

    fn render_main_view(&mut self, area: Rect, buf: &mut Buffer) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(5),
                Constraint::Length(3),
                Constraint::Length(3),
            ])
            .split(area);

        // Title
        let title = Paragraph::new(" ⏱  Time Tracker ")
            .style(Style::default().fg(Color::Cyan).bold())
            .block(Block::bordered().border_style(Style::default().fg(Color::Cyan)));
        title.render(chunks[0], buf);

        // Status section
        // Status section - show all active tasks
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
                let duration = entry.format_duration();
                lines.push(Line::from(vec![
                    Span::styled("● ", Style::default().fg(Color::Green).bold()),
                    Span::styled(format!("{}. ", i + 1), Style::default().fg(Color::DarkGray)),
                    Span::styled(&entry.activity, Style::default().fg(Color::White).bold()),
                    Span::raw(" - "),
                    Span::styled(duration, Style::default().fg(Color::Cyan).bold()),
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

        // Message area
        if let Some(ref msg) = self.message {
            let message_block = Paragraph::new(msg.as_str())
                .style(Style::default().fg(self.message_color))
                .block(Block::bordered());
            message_block.render(chunks[2], buf);
        }

        // Controls
        let controls = match self.mode {
            InputMode::Normal => {
                vec![Line::from(vec![
                    Span::styled("S", Style::default().fg(Color::Green).bold()),
                    Span::raw(" Start Task  "),
                    Span::styled("X", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop Task "),
                    Span::styled("A", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop All "),
                    Span::styled("H", Style::default().fg(Color::Yellow).bold()),
                    Span::raw(" History  "),
                    Span::styled("Q", Style::default().fg(Color::Gray).bold()),
                    Span::raw(" Quit "),
                ])]
            }
            InputMode::StartTask => {
                vec![
                    Line::from(vec![
                        Span::raw("Task name: "),
                        Span::styled(&self.input, Style::default().fg(Color::Yellow)),
                        Span::styled("█", Style::default().fg(Color::Yellow)),
                    ]),
                    Line::from(vec![
                        Span::styled("Enter", Style::default().fg(Color::Green).bold()),
                        Span::raw(" to confirm  "),
                        Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                        Span::raw(" to cancel"),
                    ]),
                ]
            }

            InputMode::StopTask => {
                vec![
                    Line::from(vec![
                        Span::raw("Task numer to stop: "),
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
            ])
            .split(area);

        // Title
        let title = Paragraph::new("Task History ")
            .style(Style::default().fg(Color::Magenta).bold())
            .block(Block::bordered().border_style(Style::default().fg(Color::Magenta)));
        title.render(chunks[0], buf);

        // History list
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
                .rev()
                .enumerate()
                .map(|(i, entry)| {
                    let duration = entry.format_duration();
                    let date = entry.start.format("%m-%d-%Y %H:%M").to_string();

                    let content = Line::from(vec![
                        Span::styled(
                            format!("{}. ", self.tracker.entries.len() - i),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(&entry.activity, Style::default().fg(Color::White).bold()),
                        Span::raw(" - "),
                        Span::styled(duration, Style::default().fg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(format!("({})", date), Style::default().fg(Color::DarkGray)),
                    ]);
                    ListItem::new(content)
                })
                .collect();
            // Delte entry for Task history.
            InputMode
                todo!()

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

        // Controls
        let controls = Paragraph::new(vec![Line::from(vec![
            Span::styled("↑↓", Style::default().fg(Color::Yellow).bold()),
            Span::raw(" Navigate  "),
            Span::styled("Esc", Style::default().fg(Color::Red).bold()),
            Span::raw(" Back to Main "),
            Span::styled("X", Style::default().fg(Color::Red).bold()),
            Span::raw(" Delete entry "),
        ])])
        .block(Block::bordered().border_style(Style::default().fg(Color::Gray)))
        .centered();
        controls.render(chunks[2], buf);
    }

    fn handle_events(&mut self) -> io::Result<()> {
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
            InputMode::StartTask => self.handle_start_task_mode(key_event),
            InputMode::StopTask => self.handle_stop_task_mode(key_event),
        }
    }

    fn handle_normal_mode(&mut self, key_event: KeyEvent) {
        match self.view {
            View::Main => match key_event.code {
                KeyCode::Char('q') | KeyCode::Char('Q') => self.exit = true,
                KeyCode::Char('s') | KeyCode::Char('S') => {
                    self.mode = InputMode::StartTask;
                    self.input.clear();
                    self.message = None;
                }
                KeyCode::Char('x') | KeyCode::Char('X') => {
                    if self.tracker.active_entries.is_empty() {
                        self.message = Some("✗ No active tasks to stop".to_string());
                        self.message_color = Color::Red;
                    } else if self.tracker.active_entries.len() == 1 {
                        // Auto-stop if only one task
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
                        // Multiple tasks - ask which to stop
                        self.mode = InputMode::StopTask;
                        self.input.clear();
                        self.message = None;
                    }
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
                _ => {}
            },
            View::History => match key_event.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('Q') => {
                    self.view = View::Main;
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
        }
    }
    fn handle_start_task_mode(&mut self, key_event: KeyEvent) {
        match key_event.code {
            KeyCode::Enter => {
                if !self.input.is_empty() {
                    match self.tracker.start(&self.input) {
                        Ok(_) => {
                            self.message = Some(format!("✓ Started tracking: {}", self.input));
                            self.message_color = Color::Green;
                            let _ = self.tracker.save();
                        }
                        Err(e) => {
                            self.message = Some(format!("✗ Error: {}", e));
                            self.message_color = Color::Red;
                        }
                    }
                }
                self.mode = InputMode::Normal;
                self.input.clear();
            }
            KeyCode::Char(c) => {
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
}
