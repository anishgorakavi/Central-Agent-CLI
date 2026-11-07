//! Terminal lifecycle and the event loop: raw mode, mouse capture, input polling and redraws.
use crate::app::App;
use crate::event::UiEvent;
use crate::ui;
use anyhow::Result;
use ratatui::DefaultTerminal;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
};
use ratatui::crossterm::execute;
use std::sync::mpsc::Receiver;
use std::time::Duration;

// Short enough for streamed text to feel live, long enough to keep idle CPU near zero.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Runs the TUI until the user quits, restoring the terminal on every exit path including panics.
pub fn run(app: &mut App, events: Receiver<UiEvent>) -> Result<()> {
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    // ratatui's own panic hook restores the screen but not mouse capture.
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture);
        previous_hook(info);
    }));
    let result = event_loop(&mut terminal, app, &events);
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

/// Drains background events, redraws, then handles at most one input event per iteration.
fn event_loop(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    events: &Receiver<UiEvent>,
) -> Result<()> {
    while !app.quit {
        while let Ok(event) = events.try_recv() {
            app.apply(event);
        }
        terminal.draw(|frame| ui::draw(frame, app))?;
        if event::poll(POLL_INTERVAL)? {
            match event::read()? {
                // Windows also reports key releases; acting on them would double every keystroke.
                Event::Key(key) if key.kind == KeyEventKind::Press => app.key(key),
                Event::Mouse(mouse) => app.mouse(mouse),
                _ => {}
            }
        }
    }
    Ok(())
}
