use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout};
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
    loop {
        terminal.draw(draw)?;

        if event::poll(Duration::from_millis(250))?
            && let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && key.code == KeyCode::Char('q')
        {
            return Ok(());
        }
    }
}

fn draw(frame: &mut Frame) {
    let rows = Layout::vertical([Constraint::Fill(1); GRID_ROWS]).split(frame.area());

    for (row_index, row_area) in rows.iter().enumerate() {
        let cols = Layout::horizontal([Constraint::Fill(1); GRID_COLS]).split(*row_area);
        for (col_index, tile_area) in cols.iter().enumerate() {
            let title = format!(" Tile {row_index},{col_index} ");
            frame.render_widget(Block::bordered().title(title), *tile_area);
        }
    }
}
