use chrono::{DateTime, Duration, Local};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    DefaultTerminal, Frame,
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    symbols::border,
    text::{Line, Span, Text},
    widgets::{Block, List, ListItem, ListState, Paragraph, Widget, Wrap},
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;

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
    current_entry: Option<TimeEntry>,
}

impl TimeTracker {
    fn new() -> Self {
        TimeTracker {
            entries: Vec::new(),
            current_entry: None,
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
        if self.current_entry.is_some() {
            return Err("Active session already running".to_string());
        }

        self.current_entry = Some(TimeEntry {
            activity: name.to_string(),
            start: Local::now(),
            end: None,
        });
        Ok(())
    }

    fn stop(&mut self) -> Result<(), String> {
        let mut entry = self
            .current_entry
            .take()
            .ok_or("No active session".to_string())?;

        entry.end = Some(Local::now());
        self.entries.push(entry);
        Ok(())
    }
}

enum InputMode {
    Normal,
    StartTask,
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
}

impl App {
    fn new() -> Self {
        let mut list_state = ListState::default();
        list_state.select(Some(0));
        
        App {
            tracker: TimeTracker::load(),
            exit: false,
            mode: InputMode::Normal,
            view: View::Main,
            input: String::new(),
            message: None,
            message_color: Color::Green,
            list_state,
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
        let status_text = match &self.tracker.current_entry {
            Some(entry) => {
                let duration = entry.format_duration();
                Text::from(vec![
                    Line::from(vec![
                        Span::styled("● ", Style::default().fg(Color::Green).bold()),
                        Span::styled("Active Task: ", Style::default().fg(Color::Yellow)),
                        Span::styled(&entry.activity, Style::default().fg(Color::White).bold()),
                    ]),
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("Started: "),
                        Span::styled(
                            entry.start.format("%Y-%m-%d %H:%M:%S").to_string(),
                            Style::default().fg(Color::Gray),
                        ),
                    ]),
                    Line::from(""),
                    Line::from(vec![
                        Span::raw("Duration: "),
                        Span::styled(duration, Style::default().fg(Color::Cyan).bold()),
                    ]),
                ])
            }
            None => Text::from(vec![
                Line::from(""),
                Line::from(vec![
                    Span::styled("○ ", Style::default().fg(Color::DarkGray)),
                    Span::styled("No active task", Style::default().fg(Color::DarkGray).italic()),
                ]),
            ]),
        };

        let status_block = Paragraph::new(status_text)
            .block(
                Block::bordered()
                    .title(" Current Status ")
                    .border_style(Style::default().fg(Color::White))
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
                vec![
                    Line::from(vec![
                        Span::styled("S", Style::default().fg(Color::Green).bold()),
                        Span::raw(" Start Task  "),
                        Span::styled("X", Style::default().fg(Color::Red).bold()),
                        Span::raw(" Stop  "),
                        Span::styled("H", Style::default().fg(Color::Yellow).bold()),
                        Span::raw(" History  "),
                        Span::styled("Q", Style::default().fg(Color::Gray).bold()),
                        Span::raw(" Quit"),
                    ])
                ]
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
        };

        let controls_block = Paragraph::new(controls)
            .block(
                Block::bordered()
                    .border_style(Style::default().fg(Color::Gray))
            )
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
        let title = Paragraph::new(" 📜 Task History ")
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
                    let date = entry.start.format("%Y-%m-%d %H:%M").to_string();
                    
                    let content = Line::from(vec![
                        Span::styled(
                            format!("{}. ", self.tracker.entries.len() - i),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::styled(&entry.activity, Style::default().fg(Color::White).bold()),
                        Span::raw(" - "),
                        Span::styled(duration, Style::default().fg(Color::Cyan)),
                        Span::raw(" "),
                        Span::styled(
                            format!("({})", date),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]);
                    ListItem::new(content)
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::bordered()
                        .title(format!(" Completed Tasks ({}) ", self.tracker.entries.len()))
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD)
                )
                .highlight_symbol("▶ ");

            ratatui::widgets::StatefulWidget::render(list, chunks[1], buf, &mut self.list_state);
        }

        // Controls
        let controls = Paragraph::new(vec![
            Line::from(vec![
                Span::styled("↑↓", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" Navigate  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Back to Main"),
            ])
        ])
        .block(Block::bordered().border_style(Style::default().fg(Color::Gray)))
        .centered();
        controls.render(chunks[2], buf);
    }

    fn handle_events(&mut self) -> io::Result<()> {
        if let Event::Key(key_event) = event::read()? {
            if key_event.kind == KeyEventKind::Press {
                self.handle_key_event(key_event);
            }
        }
        Ok(())
    }

    fn handle_key_event(&mut self, key_event: KeyEvent) {
        match self.mode {
            InputMode::Normal => self.handle_normal_mode(key_event),
            InputMode::StartTask => self.handle_input_mode(key_event),
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
                    match self.tracker.stop() {
                        Ok(_) => {
                            self.message = Some("✓ Task stopped successfully".to_string());
                            self.message_color = Color::Green;
                            let _ = self.tracker.save();
                        }
                        Err(e) => {
                            self.message = Some(format!("✗ Error: {}", e));
                            self.message_color = Color::Red;
                        }
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

    fn handle_input_mode(&mut self, key_event: KeyEvent) {
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
}
