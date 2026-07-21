mod activity;
mod config;
mod session;
mod transcript;

use std::io;
use std::path::PathBuf;
use std::thread::JoinHandle;
use std::time::Duration;

use clap::Parser;
use config::Config;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use session::{Session, SessionStatus};

const MIN_GRID_SIZE: u16 = 1;
const MAX_GRID_SIZE: u16 = 6;

/// A TUI app for running and monitoring multiple Claude Code CUI sessions in parallel.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to a config file to use instead of the default location.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Force the interactive setup screen even if a config file exists.
    #[arg(long)]
    setup: bool,
}

enum SizeField {
    Rows,
    Cols,
}

enum AppState {
    GridSize {
        rows: u16,
        cols: u16,
        field: SizeField,
    },
    TileDirs {
        rows: u16,
        cols: u16,
        dirs: Vec<String>,
        active: usize,
    },
    Grid {
        config: Config,
        sessions: Vec<TileSession>,
        /// Handles for sessions killed via 'r'/'x' in the background, so
        /// they can be joined on quit instead of risking the process
        /// exiting before an in-flight kill has actually run.
        pending_kills: Vec<JoinHandle<()>>,
        focused: (usize, usize),
        /// True while the focused tile's session is shown full-screen (#22),
        /// in which case key input is forwarded to it instead of driving
        /// grid navigation.
        floating: bool,
    },
}

/// A tile's session slot. Distinct from a plain `Option<Session>` so a
/// failed spawn (e.g. the `claude` binary is missing) can show a clear
/// in-tile error instead of looking the same as a deliberately empty tile
/// (see #26).
enum TileSession {
    Empty,
    Failed(String),
    Running(Session),
}

impl TileSession {
    fn spawn(dir: &str) -> Self {
        match Session::spawn(dir) {
            Ok(session) => TileSession::Running(session),
            Err(err) => TileSession::Failed(err.to_string()),
        }
    }
}

fn spawn_sessions(dirs: &[String]) -> Vec<TileSession> {
    dirs.iter().map(|dir| TileSession::spawn(dir)).collect()
}

/// Kills every session concurrently instead of one at a time, since each
/// kill blocks for a noticeable moment confirming the process exited, and
/// that cost would otherwise scale with the number of tiles.
fn shutdown_sessions(sessions: &mut [TileSession]) {
    std::thread::scope(|scope| {
        for slot in sessions.iter_mut() {
            if let TileSession::Running(session) = std::mem::replace(slot, TileSession::Empty) {
                scope.spawn(move || drop(session));
            }
        }
    });
}

/// Kills a single session without blocking the caller, since dropping it
/// synchronously would freeze the UI for the same noticeable moment as
/// shutdown_sessions' individual kills. The returned handle must be kept
/// (e.g. in `pending_kills`) and joined before the app exits, or the kill
/// may not have actually happened yet if the process exits first.
fn kill_in_background(session: Session) -> JoinHandle<()> {
    std::thread::spawn(move || drop(session))
}

fn default_dir() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let config_path = cli.config.or_else(config::default_path).ok_or_else(|| {
        io::Error::other("could not determine a config file location for this platform")
    })?;

    let initial_state = if !cli.setup
        && let Some(config) = config::load(&config_path)
    {
        let sessions = spawn_sessions(&config.tile_dirs);
        AppState::Grid {
            config,
            sessions,
            pending_kills: Vec::new(),
            focused: (0, 0),
            floating: false,
        }
    } else {
        AppState::GridSize {
            rows: 2,
            cols: 3,
            field: SizeField::Rows,
        }
    };

    let terminal = ratatui::init();
    let result = run(terminal, config_path, initial_state);
    ratatui::restore();
    result
}

fn run(mut terminal: DefaultTerminal, config_path: PathBuf, mut state: AppState) -> io::Result<()> {
    loop {
        terminal.draw(|frame| draw(frame, &state))?;

        if !event::poll(Duration::from_millis(250))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }

        match &mut state {
            AppState::GridSize { rows, cols, field } => match key.code {
                KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                    *field = match field {
                        SizeField::Rows => SizeField::Cols,
                        SizeField::Cols => SizeField::Rows,
                    };
                }
                KeyCode::Up => {
                    let target = match field {
                        SizeField::Rows => &mut *rows,
                        SizeField::Cols => &mut *cols,
                    };
                    *target = (*target + 1).min(MAX_GRID_SIZE);
                }
                KeyCode::Down => {
                    let target = match field {
                        SizeField::Rows => &mut *rows,
                        SizeField::Cols => &mut *cols,
                    };
                    *target = target.saturating_sub(1).max(MIN_GRID_SIZE);
                }
                KeyCode::Enter => {
                    let tile_count = *rows as usize * *cols as usize;
                    state = AppState::TileDirs {
                        rows: *rows,
                        cols: *cols,
                        dirs: vec![default_dir(); tile_count],
                        active: 0,
                    };
                }
                _ => {}
            },
            AppState::TileDirs {
                rows,
                cols,
                dirs,
                active,
            } => match key.code {
                KeyCode::Esc => {
                    state = AppState::GridSize {
                        rows: *rows,
                        cols: *cols,
                        field: SizeField::Rows,
                    };
                }
                KeyCode::Up => *active = active.saturating_sub(1),
                KeyCode::Down => *active = (*active + 1).min(dirs.len() - 1),
                KeyCode::Backspace => {
                    dirs[*active].pop();
                }
                KeyCode::Char(c) => dirs[*active].push(c),
                KeyCode::Enter => {
                    let config = Config {
                        rows: *rows as usize,
                        cols: *cols as usize,
                        tile_dirs: dirs.clone(),
                    };
                    let _ = config::save(&config_path, &config);
                    let sessions = spawn_sessions(&config.tile_dirs);
                    state = AppState::Grid {
                        config,
                        sessions,
                        pending_kills: Vec::new(),
                        focused: (0, 0),
                        floating: false,
                    };
                }
                _ => {}
            },
            AppState::Grid {
                config,
                sessions,
                pending_kills,
                focused,
                floating,
            } => {
                if *floating {
                    let is_detach = key.code == KeyCode::Char('o')
                        && key.modifiers.contains(KeyModifiers::CONTROL);
                    if is_detach {
                        *floating = false;
                    } else {
                        let index = config.tile_index(focused.0, focused.1);
                        if let Some(TileSession::Running(session)) = sessions.get(index)
                            && let Some(bytes) = key_event_to_bytes(&key)
                        {
                            let _ = session.write_input(&bytes);
                        }
                    }
                } else {
                    match key.code {
                        KeyCode::Char('q') => {
                            shutdown_sessions(sessions);
                            for handle in pending_kills.drain(..) {
                                let _ = handle.join();
                            }
                            return Ok(());
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            focused.0 = focused.0.saturating_sub(1);
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            focused.0 = (focused.0 + 1).min(config.rows - 1);
                        }
                        KeyCode::Left | KeyCode::Char('h') => {
                            focused.1 = focused.1.saturating_sub(1);
                        }
                        KeyCode::Right | KeyCode::Char('l') => {
                            focused.1 = (focused.1 + 1).min(config.cols - 1);
                        }
                        KeyCode::Char('r') => {
                            let index = config.tile_index(focused.0, focused.1);
                            if let Some(slot) = sessions.get_mut(index) {
                                if let TileSession::Running(old) =
                                    std::mem::replace(slot, TileSession::Empty)
                                {
                                    pending_kills.push(kill_in_background(old));
                                }
                                if let Some(dir) = config.tile_dirs.get(index) {
                                    *slot = TileSession::spawn(dir);
                                }
                            }
                        }
                        KeyCode::Char('x') => {
                            let index = config.tile_index(focused.0, focused.1);
                            if let Some(slot) = sessions.get_mut(index)
                                && let TileSession::Running(old) =
                                    std::mem::replace(slot, TileSession::Empty)
                            {
                                pending_kills.push(kill_in_background(old));
                            }
                        }
                        KeyCode::Enter => {
                            let index = config.tile_index(focused.0, focused.1);
                            if let Some(TileSession::Running(_)) = sessions.get(index) {
                                *floating = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

/// Translates a key event into the raw bytes a real terminal would send for
/// it, so it can be forwarded into a floated session's PTY (#22/#23). Keys
/// with no reasonable terminal encoding (e.g. function keys) are ignored.
fn key_event_to_bytes(key: &KeyEvent) -> Option<Vec<u8>> {
    if key.modifiers.contains(KeyModifiers::CONTROL)
        && let KeyCode::Char(c) = key.code
    {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_lowercase() {
            return Some(vec![lower as u8 - b'a' + 1]);
        }
    }
    match key.code {
        KeyCode::Char(c) => Some(c.to_string().into_bytes()),
        KeyCode::Enter => Some(b"\r".to_vec()),
        KeyCode::Backspace => Some(vec![0x7f]),
        KeyCode::Tab => Some(vec![0x09]),
        KeyCode::Esc => Some(vec![0x1b]),
        KeyCode::Up => Some(b"\x1b[A".to_vec()),
        KeyCode::Down => Some(b"\x1b[B".to_vec()),
        KeyCode::Right => Some(b"\x1b[C".to_vec()),
        KeyCode::Left => Some(b"\x1b[D".to_vec()),
        KeyCode::Home => Some(b"\x1b[H".to_vec()),
        KeyCode::End => Some(b"\x1b[F".to_vec()),
        KeyCode::PageUp => Some(b"\x1b[5~".to_vec()),
        KeyCode::PageDown => Some(b"\x1b[6~".to_vec()),
        KeyCode::Delete => Some(b"\x1b[3~".to_vec()),
        _ => None,
    }
}

fn draw(frame: &mut Frame, state: &AppState) {
    match state {
        AppState::GridSize { rows, cols, field } => draw_grid_size(frame, *rows, *cols, field),
        AppState::TileDirs {
            cols, dirs, active, ..
        } => draw_tile_dirs(frame, *cols, dirs, *active),
        AppState::Grid {
            config,
            sessions,
            focused,
            floating,
            ..
        } => {
            let index = config.tile_index(focused.0, focused.1);
            match (floating, sessions.get(index)) {
                (true, Some(TileSession::Running(session))) => {
                    let dir = config
                        .tile_dirs
                        .get(index)
                        .map(String::as_str)
                        .unwrap_or("");
                    draw_floating(frame, dir, session);
                }
                _ => draw_grid(frame, config, sessions, *focused),
            }
        }
    }
}

fn draw_grid_size(frame: &mut Frame, rows: u16, cols: u16, field: &SizeField) {
    let rows_label = if matches!(field, SizeField::Rows) {
        format!("> Rows: {rows} <")
    } else {
        format!("  Rows: {rows}")
    };
    let cols_label = if matches!(field, SizeField::Cols) {
        format!("> Cols: {cols} <")
    } else {
        format!("  Cols: {cols}")
    };
    let text = format!(
        "cctiles setup\n\n{rows_label}\n{cols_label}\n\nLeft/Right: switch field   Up/Down: adjust   Enter: confirm   Esc: quit"
    );
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title(" Grid size ")),
        frame.area(),
    );
}

fn draw_tile_dirs(frame: &mut Frame, cols: u16, dirs: &[String], active: usize) {
    let mut lines = vec!["cctiles setup".to_string(), String::new()];
    for (index, dir) in dirs.iter().enumerate() {
        let row = index / cols as usize;
        let col = index % cols as usize;
        let marker = if index == active { ">" } else { " " };
        let cursor = if index == active { "_" } else { "" };
        lines.push(format!("{marker} Tile {row},{col}: {dir}{cursor}"));
    }
    lines.push(String::new());
    lines.push(
        "Up/Down: switch tile   Type: edit path   Enter: confirm all   Esc: back".to_string(),
    );

    frame.render_widget(
        Paragraph::new(lines.join("\n")).block(Block::bordered().title(" Tile directories ")),
        frame.area(),
    );
}

fn draw_grid(
    frame: &mut Frame,
    config: &Config,
    sessions: &[TileSession],
    focused: (usize, usize),
) {
    let [grid_area, help_area] =
        Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(frame.area());

    let rows = Layout::vertical(vec![Constraint::Fill(1); config.rows]).split(grid_area);

    for (row_index, row_area) in rows.iter().enumerate() {
        let cols = Layout::horizontal(vec![Constraint::Fill(1); config.cols]).split(*row_area);
        for (col_index, tile_area) in cols.iter().enumerate() {
            let dir_index = config.tile_index(row_index, col_index);
            let dir = config
                .tile_dirs
                .get(dir_index)
                .map(String::as_str)
                .unwrap_or("");
            let (summary, status_color) = match sessions.get(dir_index) {
                Some(TileSession::Running(session)) => {
                    let status = session.status();
                    let summary = if status == SessionStatus::Crashed {
                        "Session ended unexpectedly. Press 'r' to restart.".to_string()
                    } else {
                        session.activity_summary()
                    };
                    let color = match status {
                        SessionStatus::Crashed => Some(Color::Magenta),
                        SessionStatus::WaitingForAnswer | SessionStatus::WaitingForPermission => {
                            Some(Color::Red)
                        }
                        SessionStatus::Normal => None,
                    };
                    (summary, color)
                }
                Some(TileSession::Failed(err)) => {
                    (format!("Failed to start: {err}"), Some(Color::Magenta))
                }
                Some(TileSession::Empty) => ("[no session]".to_string(), None),
                None => (String::new(), None),
            };
            let border_color = status_color.or(if (row_index, col_index) == focused {
                Some(Color::Yellow)
            } else {
                None
            });
            let mut block = Block::bordered().title(format!(" {dir} "));
            if let Some(color) = border_color {
                block = block.border_style(Style::default().fg(color));
            }
            let inner_area = block.inner(*tile_area);
            frame.render_widget(block, *tile_area);
            frame.render_widget(
                Paragraph::new(summary).wrap(ratatui::widgets::Wrap { trim: true }),
                inner_area,
            );
        }
    }

    frame.render_widget(
        Paragraph::new(
            "hjkl/arrows: move   enter: open terminal   r: restart tile   x: kill tile   q: quit",
        ),
        help_area,
    );
}

fn vt100_color_to_ratatui(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(index) => Color::Indexed(index),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}

/// Renders a session's live screen full-screen, matching the underlying
/// PTY's size to the rendered area every frame (#22/#24) so it always
/// reflects the real terminal window rather than the hardcoded spawn-time
/// default.
fn draw_floating(frame: &mut Frame, dir: &str, session: &Session) {
    let area = frame.area();
    let title = if session.status() == SessionStatus::Crashed {
        format!(" {dir} — session ended — Ctrl+O: back to grid ")
    } else {
        format!(" {dir} — Ctrl+O: back to grid ")
    };
    let block = Block::bordered().title(title);
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let _ = session.resize(inner.height, inner.width);

    session.with_screen(|screen| {
        let (rows, cols) = screen.size();
        let mut lines = Vec::with_capacity(rows as usize);
        for row in 0..rows {
            let mut spans = Vec::with_capacity(cols as usize);
            for col in 0..cols {
                let Some(cell) = screen.cell(row, col) else {
                    continue;
                };
                let mut style = Style::default()
                    .fg(vt100_color_to_ratatui(cell.fgcolor()))
                    .bg(vt100_color_to_ratatui(cell.bgcolor()));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                let contents = cell.contents();
                let text = if contents.is_empty() {
                    " ".to_string()
                } else {
                    contents.to_string()
                };
                spans.push(Span::styled(text, style));
            }
            lines.push(Line::from(spans));
        }
        frame.render_widget(Paragraph::new(lines), inner);

        if !screen.hide_cursor() {
            let (cursor_row, cursor_col) = screen.cursor_position();
            frame.set_cursor_position(Position {
                x: inner.x + cursor_col,
                y: inner.y + cursor_row,
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn plain_char_becomes_its_utf8_bytes() {
        assert_eq!(
            key_event_to_bytes(&key(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(b"x".to_vec())
        );
    }

    #[test]
    fn ctrl_letter_becomes_a_control_byte() {
        assert_eq!(
            key_event_to_bytes(&key(KeyCode::Char('a'), KeyModifiers::CONTROL)),
            Some(vec![0x01])
        );
    }

    #[test]
    fn enter_becomes_carriage_return() {
        assert_eq!(
            key_event_to_bytes(&key(KeyCode::Enter, KeyModifiers::NONE)),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn arrow_keys_become_ansi_escape_sequences() {
        assert_eq!(
            key_event_to_bytes(&key(KeyCode::Up, KeyModifiers::NONE)),
            Some(b"\x1b[A".to_vec())
        );
    }

    #[test]
    fn keys_with_no_reasonable_encoding_are_ignored() {
        assert_eq!(
            key_event_to_bytes(&key(KeyCode::F(5), KeyModifiers::NONE)),
            None
        );
    }
}
