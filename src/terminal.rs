use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};

pub fn setup_terminal() -> Result<ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stderr>>, Box<dyn std::error::Error>> {
    crossterm::execute!(std::io::stderr(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stderr());
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;
    Ok(terminal)
}

pub fn teardown_terminal() -> Result<(), Box<dyn std::error::Error>> {
    disable_raw_mode()?;
    crossterm::execute!(std::io::stderr(), LeaveAlternateScreen)?;
    Ok(())
}
