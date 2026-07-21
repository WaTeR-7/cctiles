use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style};
use ratatui::widgets::Block;

const GRID_ROWS: usize = 2;
const GRID_COLS: usize = 3;

fn main() -> io::Result<()> {
    let terminal = ratatui::init();
    let result = run(terminal);
    ratatui::restore();
    result
}

fn run(mut terminal: DefaultTerminal) -> io::Result<()> {
    let mut focused = (0usize, 0usize);

    loop {
        terminal.draw(|frame| draw(frame, focused))?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
        {
            match key.code {
                KeyCode::Char('q') => return Ok(()),
                KeyCode::Up | KeyCode::Char('k') => focused.0 = focused.0.saturating_sub(1),
                KeyCode::Down | KeyCode::Char('j') => {
                    focused.0 = (focused.0 + 1).min(GRID_ROWS - 1);
                }
                KeyCode::Left | KeyCode::Char('h') => focused.1 = focused.1.saturating_sub(1),
                KeyCode::Right | KeyCode::Char('l') => {
                    focused.1 = (focused.1 + 1).min(GRID_COLS - 1);
                }
                _ => {}
            }
        }
    }
}

fn draw(frame: &mut Frame, focused: (usize, usize)) {
    let rows = Layout::vertical([Constraint::Fill(1); GRID_ROWS]).split(frame.area());

    for (row_index, row_area) in rows.iter().enumerate() {
        let cols = Layout::horizontal([Constraint::Fill(1); GRID_COLS]).split(*row_area);
        for (col_index, tile_area) in cols.iter().enumerate() {
            let title = format!(" Tile {row_index},{col_index} ");
            let mut block = Block::bordered().title(title);
            if (row_index, col_index) == focused {
                block = block.border_style(Style::default().fg(Color::Yellow));
            }
            frame.render_widget(block, *tile_area);
        }
    }
}
