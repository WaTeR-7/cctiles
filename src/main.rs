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
use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph};
use session::Session;

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
        sessions: Vec<Option<Session>>,
        /// Handles for sessions killed via 'r'/'x' in the background, so
        /// they can be joined on quit instead of risking the process
        /// exiting before an in-flight kill has actually run.
        pending_kills: Vec<JoinHandle<()>>,
        focused: (usize, usize),
    },
}

fn spawn_sessions(dirs: &[String]) -> Vec<Option<Session>> {
    dirs.iter().map(|dir| Session::spawn(dir).ok()).collect()
}

/// Kills every session concurrently instead of one at a time, since each
/// kill blocks for a noticeable moment confirming the process exited, and
/// that cost would otherwise scale with the number of tiles.
fn shutdown_sessions(sessions: &mut [Option<Session>]) {
    std::thread::scope(|scope| {
        for session in sessions.iter_mut() {
            if let Some(session) = session.take() {
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
                    };
                }
                _ => {}
            },
            AppState::Grid {
                config,
                sessions,
                pending_kills,
                focused,
            } => match key.code {
                KeyCode::Char('q') => {
                    shutdown_sessions(sessions);
                    for handle in pending_kills.drain(..) {
                        let _ = handle.join();
                    }
                    return Ok(());
                }
                KeyCode::Up | KeyCode::Char('k') => focused.0 = focused.0.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    focused.0 = (focused.0 + 1).min(config.rows - 1);
                }
                KeyCode::Left | KeyCode::Char('h') => focused.1 = focused.1.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => {
                    focused.1 = (focused.1 + 1).min(config.cols - 1);
                }
                KeyCode::Char('r') => {
                    let index = config.tile_index(focused.0, focused.1);
                    if let Some(slot) = sessions.get_mut(index) {
                        if let Some(old) = slot.take() {
                            pending_kills.push(kill_in_background(old));
                        }
                        if let Some(dir) = config.tile_dirs.get(index) {
                            *slot = Session::spawn(dir).ok();
                        }
                    }
                }
                KeyCode::Char('x') => {
                    let index = config.tile_index(focused.0, focused.1);
                    if let Some(slot) = sessions.get_mut(index)
                        && let Some(old) = slot.take()
                    {
                        pending_kills.push(kill_in_background(old));
                    }
                }
                _ => {}
            },
        }
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
            ..
        } => draw_grid(frame, config, sessions, *focused),
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
    sessions: &[Option<Session>],
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
            let summary = match sessions.get(dir_index) {
                Some(Some(session)) => session.activity_summary(),
                Some(None) => "[no session]".to_string(),
                None => String::new(),
            };
            let mut block = Block::bordered().title(format!(" {dir} "));
            if (row_index, col_index) == focused {
                block = block.border_style(Style::default().fg(Color::Yellow));
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
        Paragraph::new("hjkl/arrows: move   r: restart tile   x: kill tile   q: quit"),
        help_area,
    );
}
