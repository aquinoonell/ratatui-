use chrono::{DateTime, Datelike, Duration, Local, NaiveDate};
use color_eyre::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::{
    buffer::Buffer,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span, Text},
    widgets::{
        Block, HighlightSpacing, List, ListItem, ListState, Paragraph, Row, Table, Widget, Wrap,
    },
    DefaultTerminal, Frame,
};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

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

    fn delete_task(&mut self, index: usize) -> Result<String, String> {
        if index >= self.entries.len() {
            return Err("Invalid task index".to_string());
        }
        let entry = self.entries.remove(index);
        Ok(entry.activity)
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
    StartTask,
    StopTask,
    DeleteTask,
    SelectDay,
}

enum View {
    Main,
    History,
    Calendar,
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
            View::Calendar => self.render_calendar_view(area, frame.buffer_mut()),
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

        let title = Paragraph::new(" ⏱  Time Tracker ")
            .style(Style::default().fg(Color::Cyan).bold())
            .block(Block::bordered().border_style(Style::default().fg(Color::Cyan)));
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

        if let Some(ref msg) = self.message {
            let message_block = Paragraph::new(msg.as_str())
                .style(Style::default().fg(self.message_color))
                .block(Block::bordered());
            message_block.render(chunks[2], buf);
        }

        let controls = match self.mode {
            InputMode::Normal => {
                vec![Line::from(vec![
                    Span::styled("S", Style::default().fg(Color::Green).bold()),
                    Span::raw(" Start Task  "),
                    Span::styled("X", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop Task  "),
                    Span::styled("A", Style::default().fg(Color::Red).bold()),
                    Span::raw(" Stop All  "),
                    Span::styled("C", Style::default().fg(Color::Cyan).bold()),
                    Span::raw(" Calendar  "),
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
            _ => Paragraph::new(vec![Line::from(vec![
                Span::styled("↑↓", Style::default().fg(Color::Yellow).bold()),
                Span::raw(" Navigate  "),
                Span::styled("Esc", Style::default().fg(Color::Red).bold()),
                Span::raw(" Back to Main  "),
                Span::styled("X", Style::default().fg(Color::Red).bold()),
                Span::raw(" Delete entry "),
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
                Constraint::Min(10),
                Constraint::Min(8),
                Constraint::Length(3),
            ])
            .split(area);

        let title = Paragraph::new(format!(
            "Task Calendar - {}",
            self.calendar_date.format("%Y")
        ))
        .style(Style::default().fg(Color::Cyan).bold())
        .block(Block::bordered().border_style(Style::default().fg(Color::Cyan)));
        title.render(chunks[0], buf);

        // Build calendar table
        let calendar_area = chunks[1];
        
        let now = chrono::Utc::now();
        let current_year = self.calendar_date.year();
        let current_month = self.calendar_date.month();

        // Get month name
        let month_name = chrono::Month::try_from(current_month as u8)
            .unwrap_or(chrono::Month::January)
            .name();

        // Create calendar content manually
        let calendar_lines = vec![
            Line::from(""),
            Line::from(vec![Span::styled(
                format!("{} {}", month_name, current_year),
                Style::default().fg(Color::Cyan).bold(),
            )]),
            Line::from(""),
        ];
        
        let calendar_header = Paragraph::new(calendar_lines)
            .block(Block::bordered().border_style(Style::default().fg(Color::Cyan)));
        
        // Split calendar area into header and grid
        let calendar_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(5),
                Constraint::Min(5),
            ])
            .split(calendar_area);
        
        calendar_header.render(calendar_chunks[0], buf);

        // Get calendar data
        let first_of_month = NaiveDate::from_ymd_opt(current_year, current_month, 1).unwrap();
        let last_of_month = if current_month == 12 {
            NaiveDate::from_ymd_opt(current_year + 1, 1, 1).unwrap()
        } else {
            NaiveDate::from_ymd_opt(current_year, current_month + 1, 1).unwrap()
        }
        .pred_opt()
        .unwrap();

        let first_weekday = first_of_month.weekday().num_days_from_sunday();

        // Build weekday header row
        let weekday_row = Row::new(vec![
            Span::styled("Sun           ", Style::default().fg(Color::White)),
            Span::styled("Mon           ", Style::default().fg(Color::White)),
            Span::styled("Tue           ", Style::default().fg(Color::White)),
            Span::styled("Wed           ", Style::default().fg(Color::White)),
            Span::styled("Thu           ", Style::default().fg(Color::White)),
            Span::styled("Fri           ", Style::default().fg(Color::White)),
            Span::styled("Sat           ", Style::default().fg(Color::White)),
        ]);

        // Create week rows with all 7 columns
        let mut table_rows = vec![weekday_row];
        let mut day = 1;

        for week in 0..6 {
            let mut week_cells = Vec::new();
            
            for weekday in 0..7 {
                // Empty cell before month starts or after month ends
                if (week == 0 && weekday < first_weekday) || day > last_of_month.day() {
                    week_cells.push(Span::styled("              ", Style::default()));
                } else {
                    // Check if this day has tasks
                    let day_date = NaiveDate::from_ymd_opt(current_year, current_month, day).unwrap();
                    let task_count = self.tracker.task_on_date(day_date);
                    let is_today = now.day() == day && now.month() == current_month && now.year() == current_year;
                    let is_selected = self.selected_day == Some(day_date);
                    
                    let style = if is_selected {
                        Style::default().fg(Color::Black).bg(Color::Yellow).add_modifier(Modifier::BOLD)
                    } else if is_today {
                        Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)
                    } else if task_count > 0 {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::White)
                    };
                    
                    week_cells.push(Span::styled(format!("{:<14}", day), style));
                    day += 1;
                }
            }
            
            // Only add row if it contains days
            if !week_cells.iter().all(|s| s.content.trim().is_empty()) {
                table_rows.push(Row::new(week_cells).height(1));
            }
            
            if day > last_of_month.day() {
                break;
            }
        }

        // Build table with equal width constraints for 7 columns
        let widths = [Constraint::Percentage(14); 7];
        
        let calendar_table = Table::new(table_rows, &widths)
            .column_spacing(0)
            .block(Block::bordered().border_style(Style::default().fg(Color::Cyan)));
        
        calendar_table.render(calendar_chunks[1], buf);

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
                .title(" Statistics & Tasks ")
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
            InputMode::DeleteTask => self.handle_delete_task_mode(key_event),
            InputMode::SelectDay => self.handle_select_day_mode(key_event),
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
                KeyCode::Left => {
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
                    self.selected_day = None;
                    let current_month = self.calendar_date.month() as i32;
                    let new_month = current_month - 3;

                    if new_month <= 0 {
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
                    self.selected_day = None;
                    let current_month = self.calendar_date.month() as i32;
                    let new_month = current_month + 3;

                    if new_month > 12 {
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
}
