use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::{Block, Paragraph};

const MIN_GRID_SIZE: u16 = 1;
const MAX_GRID_SIZE: u16 = 6;

struct Config {
    rows: usize,
    cols: usize,
    tile_dirs: Vec<String>,
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
        current: usize,
        input: String,
    },
    Grid {
        config: Config,
        focused: (usize, usize),
    },
}

fn default_dir() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| ".".to_string())
}

fn main() -> io::Result<()> {
    let terminal = ratatui::init();
    let result = run(terminal);
    ratatui::restore();
    result
}

fn run(mut terminal: DefaultTerminal) -> io::Result<()> {
    let mut state = AppState::GridSize {
        rows: 2,
        cols: 3,
        field: SizeField::Rows,
    };

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
                        dirs: vec![String::new(); tile_count],
                        current: 0,
                        input: default_dir(),
                    };
                }
                _ => {}
            },
            AppState::TileDirs {
                rows,
                cols,
                dirs,
                current,
                input,
            } => match key.code {
                KeyCode::Esc => return Ok(()),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(c) => input.push(c),
                KeyCode::Enter => {
                    dirs[*current] = input.clone();
                    if *current + 1 < dirs.len() {
                        *current += 1;
                        *input = default_dir();
                    } else {
                        state = AppState::Grid {
                            config: Config {
                                rows: *rows as usize,
                                cols: *cols as usize,
                                tile_dirs: dirs.clone(),
                            },
                            focused: (0, 0),
                        };
                    }
                }
                _ => {}
            },
            AppState::Grid { config, focused } => match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => focused.0 = focused.0.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    focused.0 = (focused.0 + 1).min(config.rows - 1);
                }
                KeyCode::Left | KeyCode::Char('h') => focused.1 = focused.1.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => {
                    focused.1 = (focused.1 + 1).min(config.cols - 1);
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
            rows,
            cols,
            current,
            input,
            ..
        } => draw_tile_dirs(frame, *rows, *cols, *current, input),
        AppState::Grid { config, focused } => draw_grid(frame, config, *focused),
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

fn draw_tile_dirs(frame: &mut Frame, rows: u16, cols: u16, current: usize, input: &str) {
    let row = current / cols as usize;
    let col = current % cols as usize;
    let total = rows as usize * cols as usize;
    let text = format!(
        "cctiles setup\n\nWorking directory for tile {row},{col} ({}/{total})\n\n> {input}_\n\nType a path, Enter to confirm, Esc to quit",
        current + 1
    );
    frame.render_widget(
        Paragraph::new(text).block(Block::bordered().title(" Tile directories ")),
        frame.area(),
    );
}

fn draw_grid(frame: &mut Frame, config: &Config, focused: (usize, usize)) {
    let rows = Layout::vertical(vec![Constraint::Fill(1); config.rows]).split(frame.area());

    for (row_index, row_area) in rows.iter().enumerate() {
        let cols = Layout::horizontal(vec![Constraint::Fill(1); config.cols]).split(*row_area);
        for (col_index, tile_area) in cols.iter().enumerate() {
            let dir_index = row_index * config.cols + col_index;
            let dir = config
                .tile_dirs
                .get(dir_index)
                .map(String::as_str)
                .unwrap_or("");
            let title = format!(" {dir} ");
            let mut block = Block::bordered().title(title);
            if (row_index, col_index) == focused {
                block = block.border_style(Style::default().fg(Color::Yellow));
            }
            frame.render_widget(block, *tile_area);
        }
    }
}
