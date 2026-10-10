//! Agent Mode terminal event loop: draw → pump → key handling, with the
//! terminal restored on every exit path.

use std::io;
use std::time::Duration;

use crossterm::event::{self, Event, KeyEventKind};

use crate::agent::state::AgentApp;
use crate::agent::{input, render};

/// Run the Agent TUI until the operator quits.
pub async fn run_agent(mut app: AgentApp) -> io::Result<()> {
    let mut terminal = crate::run::enter_terminal()?;
    let result = event_loop(&mut terminal, &mut app).await;
    // Fail closed: any approval the operator never answered is denied.
    app.shutdown();
    crate::run::leave_terminal(&mut terminal);
    result
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut ratatui::Terminal<B>,
    app: &mut AgentApp,
) -> io::Result<()>
where
    B::Error: Into<io::Error>,
{
    loop {
        terminal
            .draw(|frame| render::draw(frame, app))
            .map_err(Into::<io::Error>::into)?;
        if app.should_quit {
            return Ok(());
        }
        app.pump().await;
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        // Resilience contract: a key handler error is a STRUCTURED
                        // in-session error — it must never terminate the TUI. Only
                        // an explicit quit (or a truly fatal draw/IO failure)
                        // stops the process.
                        if let Err(e) = input::handle_key(app, key).await {
                            app.record_error(&format!("input error: {e}"));
                        }
                    }
                }
                // A bracketed paste arrives as ONE event carrying the whole
                // payload, not as a keystroke per character. Replaying it
                // through `handle_key` would let a pasted `\r` submit the
                // message and a pasted control byte reach the composer.
                Event::Paste(text) => input::handle_paste(app, &text),
                Event::Mouse(m) => {
                    // Track the pointer first: hit testing reads the last known
                    // position, and a terminal reports no standalone "where is the
                    // cursor" event.
                    app.set_pointer(m.column, m.row);
                    // Wheel = the matching arrow key, so every view's existing
                    // scroll semantics apply unchanged. Clicks are translated into
                    // the key they stand for (see `input::handle_click`).
                    if let Some(k) = crate::run::mouse_wheel_to_key(m.kind) {
                        if let Err(e) = input::handle_key(app, k).await {
                            app.record_error(&format!("input error: {e}"));
                        }
                    } else if let Err(e) = input::handle_click(app, m.kind).await {
                        app.record_error(&format!("input error: {e}"));
                    }
                }
                _ => {}
            }
        }
    }
}
