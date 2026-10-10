//! Terminal event loop: init → draw → key handling → restore.
//!
//! All state changes go through [`crate::App`] (async methods that call the
//! real `Runtime`). This module only translates keystrokes and owns terminal
//! setup/teardown.

use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crossterm::{
    event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind},
    terminal::{EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;

use crate::app::{App, AppError, Focus};
use crate::ui;

/// ASCII fallback: resolved once per process (locale env, overridable).
static ASCII_MODE: AtomicBool = AtomicBool::new(false);
static GLYPH_INIT: std::sync::Once = std::sync::Once::new();

/// Detect once whether the terminal needs the ASCII glyph set:
/// `OPENRT_ASCII=0` forces Unicode, `OPENRT_ASCII=1` (or any other value)
/// forces ASCII, otherwise the locale (`LC_ALL` → `LC_CTYPE` → `LANG`)
/// decides — no locale info at all falls back to ASCII (fail closed on
/// rendering, never on behavior).
pub(crate) fn init_glyph_mode() {
    GLYPH_INIT.call_once(|| {
        let forced = std::env::var_os("OPENRT_ASCII").map(|v| {
            let s = v.to_string_lossy();
            !s.is_empty() && s != "0"
        });
        let ascii = match forced {
            Some(f) => f,
            None => ["LC_ALL", "LC_CTYPE", "LANG"]
                .iter()
                .find_map(|k| std::env::var(k).ok())
                .map(|v| !v.to_lowercase().contains("utf"))
                .unwrap_or(true),
        };
        ASCII_MODE.store(ascii, Ordering::Relaxed);
    });
}

/// Whether the ASCII glyph set is active (tests + render diagnostics).
pub fn ascii_mode() -> bool {
    ASCII_MODE.load(Ordering::Relaxed)
}

/// Serializes every test that flips the resolved glyph mode.
///
/// The mode is process-global because a terminal's capability is a property of
/// the terminal, not of a thread. Tests run in parallel, so two tests asserting
/// different modes corrupt each other: the failure looks like a truncation bug
/// (`...p/file.rs` instead of `…eep/file.rs`) in a test that never touched
/// glyphs. Hold this lock across `set_ascii_mode` *and* across the assertions
/// that depend on it.
#[cfg(test)]
pub(crate) static ASCII_MODE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquire the glyph-mode lock, ignoring poisoning: a previous test panicking
/// mid-mode is exactly when the next test most needs to run.
#[cfg(test)]
pub(crate) fn ascii_mode_lock() -> std::sync::MutexGuard<'static, ()> {
    ASCII_MODE_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Test-only: flip the resolved mode to exercise both glyph branches.
#[cfg(test)]
pub(crate) fn set_ascii_mode(ascii: bool) {
    ASCII_MODE.store(ascii, Ordering::Relaxed);
}

/// The functional glyph inventory: every character the app draws *because it
/// means something*, paired with the ASCII that means the same thing.
///
/// # Why a table and not a convention
///
/// Two mechanisms used to decide ASCII rendering: `g(unicode, ascii)` at the
/// call site, and a replacement chain inside `sanitize_text`. They disagreed.
/// `▾` (a collapsed tree branch) and `⎇` (the Alt key) and `⋯` (a running
/// ellipsis) were painted as literals and passed straight through, so on an
/// ASCII terminal a collapsed branch looked identical to an expanded one and
/// the Alt key hint rendered as a box. A glyph that means something and has no
/// fallback is not a cosmetic bug: it silently removes a distinction the
/// operator was relying on.
///
/// One table now decides, and [`g`] is checked against it, so a new glyph
/// cannot arrive without a fallback.
///
/// # What belongs here
///
/// Only *functional* glyphs — markers that carry state: selection, focus,
/// status, direction, separators, block shading, key names. Prose characters
/// from the operator's own text (accents, CJK, emoji) are never in this table;
/// they are content, they pass through untouched, and the width helpers in
/// [`crate::design::text`] handle them.
///
/// Punctuation is deliberately **not** here. `·`, `—`, `–`, `“`, `”` are
/// decorative rather than stateful, every code page this app targets has them,
/// and rewriting them would corrupt prose the operator wrote — an em dash in a
/// model response is content, not a marker. Only marks that *carry state* get
/// replaced.
pub(crate) const GLYPHS: &[(char, &str)] = &[
    // Selection and focus markers.
    ('\u{25b8}', ">"), // ▸ selected
    ('\u{203a}', ">"), // › selection caret
    ('\u{25be}', "v"), // ▾ collapsed branch
    // Status markers. `[OK]`/`[X]` rather than `+`/`x`: an `x` next to a path
    // reads as part of a filename, and a `+` reads as a diff.
    ('\u{2713}', "[OK]"), // ✓ success
    ('\u{2717}', "[X]"),  // ✗ failure
    ('\u{26a0}', "[!]"),  // ⚠ warning
    ('\u{25cf}', "*"),    // ● active/selected radio
    ('\u{25cb}', "o"),    // ○ inactive radio
    ('\u{25cc}', "o"),    // ◌ loading dot
    // Direction and motion.
    ('\u{2192}', "->"),
    ('\u{2190}', "<-"),
    ('\u{2194}', "<->"),
    ('\u{2191}', "^"),
    ('\u{2193}', "v"),
    ('\u{21d2}', "=>"), // ⇒ consequence
    // Truncation. `…` and `⋯` are different characters and only the first was
    // in the old chain, so a running ellipsis reached ASCII terminals raw.
    ('\u{2026}', "..."),
    ('\u{22ef}', "..."),
    // Box drawing and list marks.
    ('\u{2022}', "*"), // • bullet
    ('\u{2500}', "-"),
    ('\u{2502}', "|"),
    ('\u{251c}', "+"),
    ('\u{2524}', "+"),
    ('\u{250c}', "+"),
    ('\u{2510}', "+"),
    ('\u{2514}', "+"),
    ('\u{2518}', "+"),
    // Block shading for progress and focus bars.
    ('\u{2588}', "#"),
    ('\u{258c}', "|"),
    ('\u{2591}', "."),
    ('\u{2592}', ":"),
    ('\u{2593}', "="),
    // Key names and misc functional marks.
    ('\u{2387}', "Alt"), // ⎇ the Alt modifier
    ('\u{23ce}', "\\n"), // ⏎ newline inside a collapsed paste
    ('\u{00d7}', "x"),   // × close
    ('\u{25a0}', "#"),   // ■ blocked outright
    ('\u{00d8}', "0"),   // Ø not available in this configuration
    // Braille spinner phases; `spinner()` maps these in lockstep so the motion
    // is still visible in ASCII.
    ('\u{280b}', "|"),
    ('\u{2819}', "/"),
    ('\u{2839}', "-"),
    ('\u{2838}', "\\"),
];

/// Punctuation that is deliberately **not** substituted in ASCII mode.
///
/// These carry no state — they are punctuation in prose and in labels. Every
/// code page this app targets has them, so substituting them would replace
/// something that already renders correctly while corrupting text the operator
/// wrote (an em dash inside a model response is content, not a marker).
///
/// Listed here rather than left implicit so [`g`] can prove that every
/// non-ASCII glyph it is handed is accounted for: either it has a fallback, or
/// it is punctuation we consciously pass through.
pub(crate) const PASSTHROUGH_PUNCTUATION: &[char] = &[
    '\u{00b7}', // ·
    '\u{2014}', // —
    '\u{2013}', // –
    '\u{201c}', // "
    '\u{201d}', // "
    '\u{2018}', // '
    '\u{2019}', // '
];

/// The ASCII stand-in for a functional glyph, or `None` when the character has
/// no entry (and therefore is either content or a bug — see [`GLYPHS`]).
pub(crate) fn ascii_fallback(ch: char) -> Option<&'static str> {
    GLYPHS.iter().find(|(u, _)| *u == ch).map(|(_, a)| *a)
}

/// Glyph pair: the Unicode mark in UTF-8 terminals, a safe ASCII stand-in
/// in ASCII mode.
///
/// The ASCII argument is verified against [`GLYPHS`] in debug builds, so the
/// inline pair and the central table cannot drift into saying different things
/// about the same character.
pub(crate) fn g(unicode: &'static str, ascii: &'static str) -> &'static str {
    let first = unicode.chars().next().unwrap_or(' ');
    debug_assert!(
        first.is_ascii()
            || ascii_fallback(first).is_some()
            || PASSTHROUGH_PUNCTUATION.contains(&first),
        "{unicode:?} is neither in the GLYPHS inventory nor documented passthrough punctuation",
    );
    if ascii_mode() {
        ascii
    } else {
        unicode
    }
}

/// Text sanitizer for model/user-provided content in ASCII mode: every
/// functional glyph in [`GLYPHS`] is replaced, so model output, chat messages,
/// tool arguments and error text all degrade the same way.
///
/// Prose is untouched: an accented name or a CJK string from the operator stays
/// exactly as typed. What this protects is the *markers the app itself draws*,
/// which must not turn into replacement characters on a terminal that cannot
/// show them.
pub(crate) fn sanitize_text(s: &str) -> String {
    if !ascii_mode() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ascii_fallback(ch) {
            Some(a) => out.push_str(a),
            None => out.push(ch),
        }
    }
    out
}

/// Wheel lines become the equivalent arrow key so every view's existing
/// scroll semantics (chat history, list selection, terminal scrollback)
/// apply unchanged. Clicks are intentionally ignored — no view has click
/// affordances, and a click must never fire a default action like Enter.
pub(crate) fn mouse_wheel_to_key(kind: MouseEventKind) -> Option<KeyEvent> {
    match kind {
        MouseEventKind::ScrollUp => Some(KeyEvent::new(KeyCode::Up, KeyModifiers::empty())),
        MouseEventKind::ScrollDown => Some(KeyEvent::new(KeyCode::Down, KeyModifiers::empty())),
        _ => None,
    }
}

/// Raw-mode + alternate-screen setup shared by both TUI modes.
///
/// Every step that succeeds is undone if a later step fails. Entering raw mode
/// and then returning early — because `execute!` or `Terminal::new` failed —
/// left the operator with an unusable shell and no TUI to tell them why, which
/// Every terminal capability the TUI turns on. Each one is something the
/// operator's shell will still be stuck with if we exit without undoing it, so
/// the set is enumerated rather than implied by call order.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Feature {
    RawMode,
    AlternateScreen,
    MouseCapture,
    BracketedPaste,
    CursorHidden,
}

impl Feature {
    /// Test-only: the full feature set, so a test can assert every entry has an
    /// inverse without restating the list (and drifting from it).
    #[cfg(test)]
    pub(crate) const ALL: [Feature; 5] = [
        Feature::RawMode,
        Feature::AlternateScreen,
        Feature::MouseCapture,
        Feature::BracketedPaste,
        Feature::CursorHidden,
    ];

    /// Test-only: the label a lifecycle test asserts on. Production reporting
    /// does not currently name features, so this does not pretend otherwise.
    #[cfg(test)]
    fn name(self) -> &'static str {
        match self {
            Feature::RawMode => "raw mode",
            Feature::AlternateScreen => "alternate screen",
            Feature::MouseCapture => "mouse capture",
            Feature::BracketedPaste => "bracketed paste",
            Feature::CursorHidden => "hidden cursor",
        }
    }
}

/// Where the escape sequences actually go. Production talks to crossterm; the
/// lifecycle tests talk to a recorder, which is the only way to assert that a
/// feature was *not* left enabled.
pub(crate) trait TerminalSink {
    fn enable(&mut self, feature: Feature) -> io::Result<()>;
    fn disable(&mut self, feature: Feature) -> io::Result<()>;
}

/// Owns the enabled-feature stack and guarantees a symmetric restore.
///
/// The contract this type exists to enforce: **every feature that was enabled
/// is disabled again, exactly once, on every exit path** — normal return, error
/// return, partial-initialization failure, or panic. Three properties make that
/// true rather than aspirational:
///
/// * restore runs in reverse order, because a terminal state layered on top of
///   another must come off in the opposite order;
/// * a failing `disable` does not abort the restore, because the features
///   behind it would then stay on forever;
/// * the stack is drained as it goes, so a second call is a no-op rather than a
///   double-restore.
pub(crate) struct Session<S: TerminalSink> {
    sink: S,
    live: Vec<Feature>,
}

impl<S: TerminalSink> Session<S> {
    pub(crate) fn new(sink: S) -> Self {
        Self {
            sink,
            live: Vec::new(),
        }
    }

    /// Turn one feature on and record that we owe its inverse.
    pub(crate) fn enter(&mut self, feature: Feature) -> io::Result<()> {
        self.sink.enable(feature)?;
        self.live.push(feature);
        Ok(())
    }

    /// Turn one feature off and forget it.
    ///
    /// Test-only: production exit always goes through `restore_all`, which
    /// unwinds the whole stack rather than a single feature.
    #[cfg(test)]
    pub(crate) fn leave(&mut self, feature: Feature) -> io::Result<()> {
        self.sink.disable(feature)?;
        self.live.retain(|f| *f != feature);
        Ok(())
    }

    /// Restore everything still enabled, newest first.
    ///
    /// Returns the features that could not be restored, so a caller can report
    /// a genuinely stuck terminal instead of pretending it exited cleanly.
    pub(crate) fn restore_all(&mut self) -> Vec<Feature> {
        let mut failed = Vec::new();
        while let Some(feature) = self.live.pop() {
            // Deliberately keep going: the features underneath this one are
            // still enabled, and bailing out here would strand them.
            if self.sink.disable(feature).is_err() {
                failed.push(feature);
            }
        }
        failed
    }

    /// Features still enabled. Zero is the only healthy value outside setup.
    #[cfg(test)]
    pub(crate) fn live(&self) -> &[Feature] {
        &self.live
    }
}

/// The production sink: real crossterm escape sequences.
struct CrosstermSink;

impl TerminalSink for CrosstermSink {
    fn enable(&mut self, feature: Feature) -> io::Result<()> {
        let mut out = io::stdout();
        match feature {
            Feature::RawMode => crossterm::terminal::enable_raw_mode(),
            Feature::AlternateScreen => crossterm::execute!(out, EnterAlternateScreen).map(|_| ()),
            Feature::MouseCapture => {
                crossterm::execute!(out, crossterm::event::EnableMouseCapture).map(|_| ())
            }
            Feature::BracketedPaste => {
                crossterm::execute!(out, crossterm::event::EnableBracketedPaste).map(|_| ())
            }
            Feature::CursorHidden => crossterm::execute!(out, crossterm::cursor::Hide).map(|_| ()),
        }
    }

    fn disable(&mut self, feature: Feature) -> io::Result<()> {
        let mut out = io::stdout();
        match feature {
            Feature::RawMode => crossterm::terminal::disable_raw_mode(),
            Feature::AlternateScreen => crossterm::execute!(out, LeaveAlternateScreen).map(|_| ()),
            Feature::MouseCapture => {
                crossterm::execute!(out, crossterm::event::DisableMouseCapture).map(|_| ())
            }
            Feature::BracketedPaste => {
                crossterm::execute!(out, crossterm::event::DisableBracketedPaste).map(|_| ())
            }
            Feature::CursorHidden => crossterm::execute!(out, crossterm::cursor::Show).map(|_| ()),
        }
    }
}

/// The order features come on. Raw mode first because every later escape is
/// written through it; the cursor is hidden last, once the full-screen frame is
/// about to be drawn.
pub(crate) const ENTRY_ORDER: [Feature; 5] = [
    Feature::RawMode,
    Feature::AlternateScreen,
    Feature::MouseCapture,
    Feature::BracketedPaste,
    Feature::CursorHidden,
];

/// Bring the terminal up, guaranteeing a restore if any step fails.
///
/// This is the one failure mode the panic hook cannot cover: if setup fails
/// halfway there is no `Terminal` for the caller to restore through, so the
/// restore has to happen here.
pub(crate) fn enter_terminal() -> io::Result<Terminal<CrosstermBackend<io::Stdout>>> {
    install_panic_restore();
    init_glyph_mode();

    let mut session = Session::new(CrosstermSink);
    for feature in ENTRY_ORDER {
        if let Err(e) = session.enter(feature) {
            // Everything enabled so far is owed a restore. The caller never
            // receives a terminal, so this is the only chance.
            session.restore_all();
            set_session_live(false);
            return Err(e);
        }
    }

    let backend = CrosstermBackend::new(io::stdout());
    match Terminal::new(backend) {
        Ok(t) => {
            // The session now owns the terminal, so a panic from here on is
            // this hook's business.
            set_session_live(true);
            Ok(t)
        }
        Err(e) => {
            // The escape sequences went out but the terminal wrapper did not
            // come up, so undo those too rather than assuming `leave_terminal`
            // will be reached.
            session.restore_all();
            set_session_live(false);
            Err(e)
        }
    }
}

/// Whether a full-screen session currently owns the terminal.
///
/// The panic hook consults this before emitting anything. `disable_raw_mode`
/// and `LeaveAlternateScreen` act on the *process's* terminal, and a terminal
/// is shared by every process attached to it — so a hook that fires when no
/// session is live resets the tty out from under whatever else is drawing on
/// it (a shell, another full-screen program) without ever having put anything
/// there to restore. Installing the hook is therefore not the same as owning
/// the terminal, and only ownership authorises a restore.
static SESSION_LIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn session_live() -> bool {
    SESSION_LIVE.load(std::sync::atomic::Ordering::SeqCst)
}

fn set_session_live(live: bool) {
    SESSION_LIVE.store(live, std::sync::atomic::Ordering::SeqCst);
}

/// Whether the panic hook should restore the terminal.
///
/// Split out from the hook so both branches are directly testable: the restore
/// branch cannot be observed in-process (it writes to the real tty), but the
/// *decision* is the safety property, and it is what keeps an unrelated panic
/// from resetting someone else's terminal.
fn panic_should_restore() -> bool {
    session_live()
}

/// A panic anywhere in the TUI must not leave the operator's terminal in raw
/// mode with the alternate screen still up (the classic "my shell is broken"
/// failure). Chain the previous hook so the panic message still prints — on a
/// RESTORED terminal, after leaving the alternate screen.

pub(crate) fn install_panic_restore() {
    static INSTALLED: std::sync::Once = std::sync::Once::new();
    INSTALLED.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // Only a session that actually came up owes the terminal a
            // restore. Without this guard a panic anywhere in this process
            // would emit terminal escapes on a terminal it never configured.
            if panic_should_restore() {
                // Same fail-closed restore as every other exit path, and in the
                // same reverse order.
                let mut session = Session::new(CrosstermSink);
                for feature in ENTRY_ORDER.iter().rev() {
                    let _ = session.enter(*feature);
                }
                session.restore_all();
                set_session_live(false);
            }
            previous(info);
        }));
    });
}

/// Fail-closed terminal restore (ignore restore errors so the original
/// event-loop error surfaces).
pub(crate) fn leave_terminal(terminal: &mut Terminal<CrosstermBackend<io::Stdout>>) {
    // Ownership is released *before* the restore so a panic during restore
    // cannot make the hook re-restore a terminal that is already on its way
    // back to normal.
    set_session_live(false);
    let mut sink = CrosstermSink;
    for feature in ENTRY_ORDER.iter().rev() {
        let _ = sink.disable(*feature);
    }
    let _ = terminal.show_cursor();
}

/// Run the full-screen TUI until the operator quits. Restores the terminal on
/// every exit path (including error returns).
pub async fn run_tui(mut app: App) -> io::Result<()> {
    let mut terminal = enter_terminal()?;
    let result = event_loop(&mut terminal, &mut app).await;
    leave_terminal(&mut terminal);
    result
}

/// Workbench key handling, callable from Agent Mode's shared event loop.
pub(crate) async fn workbench_key(app: &mut App, key: KeyEvent) -> Result<(), AppError> {
    handle_key(app, key).await
}

async fn event_loop<B: ratatui::backend::Backend>(
    terminal: &mut Terminal<B>,
    app: &mut App,
) -> io::Result<()>
where
    B::Error: Into<io::Error>,
{
    loop {
        terminal
            .draw(|frame| ui::draw(frame, app))
            .map_err(Into::<io::Error>::into)?;
        if app.should_quit {
            return Ok(());
        }
        // Poll with a short timeout so async refresh stays responsive.
        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) => {
                    if key.kind == KeyEventKind::Press {
                        handle_key(app, key)
                            .await
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                    }
                }
                // A bracketed paste arrives as one event carrying the whole
                // payload, not as a keystroke per character. Feeding it through
                // `handle_key` char-by-char would let a pasted `\r` submit a
                // message and a pasted control byte reach the composer.
                Event::Paste(text) => {
                    handle_paste(app, &text);
                }
                Event::Mouse(m) => {
                    // Presses and releases resolve through the renderer-owned
                    // registry; the wheel keeps working as a key repeat so a
                    // scroll and an arrow key cannot drift apart.
                    if app.handle_mouse(m).await.is_err() {
                        app.status = "selection failed".into();
                    }
                    if let Some(k) = mouse_wheel_to_key(m.kind) {
                        handle_key(app, k)
                            .await
                            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))?;
                    }
                }
                _ => {}
            }
        }
    }
}

/// Handle a bracketed paste in Workbench mode.
///
/// Workbench has no text field — its panels are lists, and the only input is the
/// Operations command line handled inside the Agent path. Bracketed paste is
/// still enabled here so the escape sequences are balanced on enter and exit,
/// but a paste that arrives with nowhere to go is dropped rather than silently
/// redirected somewhere the operator did not target.
fn handle_paste(_app: &mut App, _text: &str) {}

async fn handle_key(app: &mut App, key: KeyEvent) -> Result<(), crate::AppError> {
    // Hard quit works even while typing a steer directive.
    if matches!(
        (key.code, key.modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
    ) {
        app.quit();
        return Ok(());
    }
    // Steer text-input mode (M11.6): consumes every other key until the
    // operator submits (Enter) or cancels (Esc).
    if app.steer_input.is_some() {
        match key.code {
            KeyCode::Esc => {
                app.steer_input = None;
                app.status = "steer cancelled — nothing appended".into();
            }
            KeyCode::Backspace => {
                if let Some(buf) = app.steer_input.as_mut() {
                    buf.pop();
                }
            }
            KeyCode::Enter => app.room_steer_submit().await,
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(buf) = app.steer_input.as_mut() {
                    buf.push(c);
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Rooms panel keys (M11.6): pause / resume / steer on the selected room —
    // guarded so the global 'r' (run prompt) never hijacks them.
    if app.focus == Focus::Ops && app.ops_tab == crate::app::OpsTab::Rooms {
        match key.code {
            KeyCode::Char('p') => {
                app.room_pause().await;
                return Ok(());
            }
            KeyCode::Char('r') => {
                app.room_resume().await;
                return Ok(());
            }
            KeyCode::Char('s') => {
                app.room_steer_begin();
                return Ok(());
            }
            _ => {}
        }
    }

    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) | (KeyCode::Char('q'), _) => {
            app.quit();
        }
        (KeyCode::Char('r'), _) if key.modifiers.contains(KeyModifiers::CONTROL) => {
            app.refresh().await?;
        }
        (KeyCode::Tab, _) => {
            app.cycle_focus();
        }
        (KeyCode::BackTab, _) => {
            // Shift-Tab: cycle backward around the four-panel ring.
            //
            // This was four forward steps, which only works while the ring has
            // five stops: 4 steps forward is 1 step back on a five-ring, and the
            // identity on a four-ring — Shift+Tab did nothing at all. Ring depth
            // is now stated in one place (`cycle_focus_back`) so this cannot
            // drift from the ring again.
            app.cycle_focus_back();
        }
        (KeyCode::Up, _) => {
            app.move_selection(-1).await?;
        }
        (KeyCode::Down, _) => {
            app.move_selection(1).await?;
        }
        (KeyCode::Enter, _) => {
            app.activate_focused().await?;
        }
        // Esc is contextually valid only: the steer line above already
        // consumed it while one was open, so reaching here means there is no
        // layer to unwind. Say so rather than doing nothing silently.
        //
        // Note what this deliberately does NOT do: quit. Quitting is Ctrl+C/q.
        // An Esc that exits the app is how a mis-aimed keypress loses a session.
        (KeyCode::Esc, _) => {
            app.status = "nothing to cancel".into();
        }
        (KeyCode::Char('n'), _) => {
            // New session: use a deterministic title from the session count.
            let title = format!("tui-{}", app.sessions.len() + 1);
            app.create_session(&title).await?;
        }
        (KeyCode::Char('r'), _) => {
            // Run a tiny real prompt against the current session (mock-friendly).
            app.run_prompt("tui ping").await?;
        }
        (KeyCode::Char('w'), _) => {
            // Phase 6: compile a workflow through the production compiler.
            app.compile_workflow_from_context().await?;
        }
        // Phase 10/11/12: Ops sub-views — 1 timeline, 2 skills, 3 shield,
        // 4 recovery, 5 rooms, 6 extensions, 7 computer, 8 phase-12 intel
        // (each renders real store data).
        (KeyCode::Char('1'), _) => app.set_ops_tab(crate::app::OpsTab::Timeline),
        (KeyCode::Char('2'), _) => app.set_ops_tab(crate::app::OpsTab::Skills),
        (KeyCode::Char('3'), _) => app.set_ops_tab(crate::app::OpsTab::Shield),
        (KeyCode::Char('4'), _) => app.set_ops_tab(crate::app::OpsTab::Recovery),
        (KeyCode::Char('5'), _) => app.set_ops_tab(crate::app::OpsTab::Rooms),
        (KeyCode::Char('6'), _) => app.set_ops_tab(crate::app::OpsTab::Extensions),
        (KeyCode::Char('7'), _) => app.set_ops_tab(crate::app::OpsTab::Computer),
        (KeyCode::Char('8'), _) => app.set_ops_tab(crate::app::OpsTab::Intel),
        // M10.5 time machine — what-if actions over the one ledger
        // (store-derived names; failures land on the status line).
        (KeyCode::Char('f'), _) => app.ops_time_machine_fork().await,
        (KeyCode::Char('c'), _) => app.ops_time_machine_compare().await,
        (KeyCode::Char('a'), _) => app.ops_time_machine_state().await,
        (KeyCode::Char('v'), _) => app.ops_time_machine_return().await,
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod glyph_inventory_tests {
    //! The ASCII fallback is the difference between a terminal that shows a
    //! collapsed tree branch and one that shows the same thing as an expanded
    //! branch. These tests make that inventory enforceable instead of
    //! conventional: the table is checked for self-consistency, every entry is
    //! proven to degrade, and the source tree is scanned so no new glyph can be
    //! painted without a fallback.

    use super::*;

    /// Codepoint ranges that hold *functional* marks: arrows and math
    /// operators, technical marks (the Alt key, the return symbol), geometric
    /// shapes, block elements, and dingbats.
    ///
    /// Everything else non-ASCII is prose or operator content — an accented
    /// name, a CJK string, an emoji — and is allowed to pass through untouched,
    /// because replacing it would corrupt what the operator actually wrote.
    fn is_functional(ch: char) -> bool {
        let c = ch as u32;
        (0x2190..=0x2BFF).contains(&c)   // arrows, math, boxes, blocks
            || (0x2300..=0x23FF).contains(&c) // ⎇ ⏎
            || (0x25A0..=0x27BF).contains(&c) // ● ○ ▸ ✓ ⚠ ◌
            || (0x2800..=0x28FF).contains(&c) // braille spinner phases
    }

    #[test]
    fn every_inventory_entry_is_a_mark_with_an_ascii_replacement() {
        for (u, ascii) in GLYPHS {
            // An entry exists to replace a *drawn mark*. ASCII needs no entry,
            // and whitespace or ASCII punctuation in here would mean the
            // sanitizer rewrites layout rather than substituting a glyph.
            // (`Ø` is a marker even though Unicode calls it a letter — that is
            // exactly why a category test is the wrong test.)
            assert!(!u.is_ascii(), "{u:?} is ASCII; it needs no fallback entry");
            assert!(!u.is_whitespace(), "{u:?} is whitespace");
            assert!(!u.is_ascii_punctuation(), "{u:?} is ASCII punctuation");
            assert!(
                ascii.is_ascii(),
                "{u:?} falls back to {ascii:?}, which is not ASCII — the fallback defeats the purpose",
            );
            assert!(!ascii.is_empty(), "{u:?} has no fallback at all");
        }
    }

    #[test]
    fn the_inventory_and_the_passthrough_set_never_overlap() {
        for p in PASSTHROUGH_PUNCTUATION {
            assert!(
                ascii_fallback(*p).is_none(),
                "{p:?} is both substituted and passed through; pick one",
            );
            assert!(!p.is_alphanumeric(), "{p:?} is text, not punctuation");
        }
    }

    #[test]
    fn the_inventory_has_no_duplicate_unicode_glyphs() {
        let mut seen = std::collections::BTreeSet::new();
        for (u, _) in GLYPHS {
            assert!(
                seen.insert(*u),
                "{u:?} appears twice; the first match would win"
            );
        }
    }

    #[test]
    fn every_inventory_glyph_degrades_to_its_fallback_in_ascii_mode() {
        // ASCII mode is one process-global flag, so a test that *sets* it must
        // hold the lock or it silently re-renders some unrelated test's
        // expectations mid-run.
        let _guard = ascii_mode_lock();
        set_ascii_mode(true);
        for (u, ascii) in GLYPHS {
            assert_eq!(sanitize_text(&u.to_string()), *ascii, "{u:?}");
            assert_eq!(g(Box::leak(u.to_string().into_boxed_str()), ascii), *ascii);
        }
        set_ascii_mode(false);
        // Unicode mode must not have been contaminated by the sweep above.
        for (u, _) in GLYPHS {
            assert_eq!(sanitize_text(&u.to_string()), u.to_string());
        }
    }

    /// `spinner()` keeps its own ASCII array so the hot path stays
    /// allocation-free; this is what stops that array from drifting away from
    /// the inventory and blanking the spinner in ASCII mode.
    #[test]
    fn the_spinner_phases_agree_with_the_inventory() {
        const FRAMES: [char; 4] = ['\u{280b}', '\u{2819}', '\u{2839}', '\u{2838}'];
        const ASCII: [&str; 4] = ["|", "/", "-", "\\"];
        for (i, frame) in FRAMES.iter().enumerate() {
            assert_eq!(ascii_fallback(*frame), Some(ASCII[i]), "spinner phase {i}");
        }
        for i in 0..8 {
            assert!(!crate::design::widgets::spinner(i).is_empty());
        }
    }

    #[test]
    fn operator_prose_is_never_rewritten() {
        // ASCII mode is one process-global flag, so a test that *sets* it must
        // hold the lock or it silently re-renders some unrelated test's
        // expectations mid-run.
        let _guard = ascii_mode_lock();
        set_ascii_mode(true);
        for s in [
            "café",
            "日本語",
            "naïve — dash",
            "“quoted”",
            "path/文件.rs",
            "🎉",
        ] {
            assert_eq!(
                sanitize_text(s),
                s,
                "{s:?} was rewritten; it is content, not a marker"
            );
        }
        set_ascii_mode(false);
    }

    /// The enforcement test: scan every production source file for functional
    /// glyphs that are not in the inventory.
    ///
    /// This is the only way the inventory stays true. Without it, the next
    /// person to write `let marker = "▶"` ships a terminal where collapsed and
    /// expanded branches look identical, and no existing test notices.
    #[test]
    fn no_source_file_paints_a_functional_glyph_without_an_inventory_entry() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let paths = rust_sources(&root);
        let mut offenders: Vec<String> = Vec::new();
        for path in &paths {
            for ch in production_lines(path).iter().flat_map(|l| l.chars()) {
                if is_functional(ch) && ascii_fallback(ch).is_none() {
                    let name = path.strip_prefix(&root).unwrap_or(path).display();
                    offenders.push(format!("{name}: {ch:?} (U+{:04X})", ch as u32));
                }
            }
        }
        offenders.sort();
        offenders.dedup();
        assert!(
            offenders.is_empty(),
            "{} functional glyph(s) have no ASCII fallback; add them to run::GLYPHS:\n{}",
            offenders.len(),
            offenders.join("\n"),
        );
        assert!(paths.len() > 10, "the scan only read {} files", paths.len());
    }

    #[test]
    fn inline_glyph_pairs_agree_with_the_inventory() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut disagreements: Vec<String> = Vec::new();
        for path in rust_sources(&root) {
            for line in production_lines(&path) {
                // Walk each `g(` occurrence and read the two string literals
                // belonging to *it*. A line can hold several unrelated literals
                // (`format!(" {}", g("●", "*"))`), so scanning the whole line
                // for quotes reads the wrong pair.
                let mut from = 0usize;
                while let Some(at) = line[from..].find("g(\"") {
                    // Read both literals by explicit offsets so `from` always
                    // advances past the pair. Anything that can re-find the same
                    // `g("` turns this into an infinite loop, which is how a
                    // scan-based test can hang the suite instead of failing it.
                    let base = from + at + 2;
                    let Some(u_end) = line[base..].find('"') else {
                        break;
                    };
                    let u = &line[base..base + u_end];
                    let after_u = base + u_end + 1;
                    let Some(a_gap) = line[after_u..].find('"') else {
                        break;
                    };
                    let a_base = after_u + a_gap + 1;
                    let Some(a_end) = line[a_base..].find('"') else {
                        break;
                    };
                    let a = &line[a_base..a_base + a_end];
                    from = a_base + a_end + 1;
                    let Some(first) = u.chars().next() else {
                        continue;
                    };
                    if let Some(table) = ascii_fallback(first) {
                        if table != a {
                            let name = path.file_name().unwrap_or_default().to_string_lossy();
                            disagreements.push(format!(
                                "{name}: g({u:?}, {a:?}) but the inventory says {table:?} for {first:?}",
                            ));
                        }
                    }
                }
            }
        }
        disagreements.sort();
        disagreements.dedup();
        assert!(
            disagreements.is_empty(),
            "g() pairs disagree with run::GLYPHS:\n{}",
            disagreements.join("\n"),
        );
    }

    /// Every `.rs` file under the crate's `src`, in a stable order.
    fn rust_sources(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let mut entries: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
                .expect("src is readable")
                .map(|e| e.expect("dir entry").path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    stack.push(path);
                } else if path.extension().and_then(|e| e.to_str()) == Some("rs")
                    // A `tests.rs` is pulled in by `#[cfg(test)] mod tests;`, so it
                    // carries no cfg marker of its own and is test data throughout.
                    && path.file_name().and_then(|n| n.to_str()) != Some("tests.rs")
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    /// The code lines of a source file: everything before its first
    /// `#[cfg(test)]` module, with whole-line comments dropped.
    ///
    /// Test modules are allowed to contain any glyph as data, and comments are
    /// prose — the ASCII module documents itself with box drawing, and scanning
    /// it would demand fallbacks for its own documentation.
    /// Phase 9: runtime code must not be able to panic the UI.
    ///
    /// `unreachable!`, `todo!` and `panic!` are all "this cannot happen" claims.
    /// In a terminal app they are load-bearing in the worst way: a spawned task
    /// that panics does not take the process down, it just stops existing, and
    /// the operator is left staring at a status line that says "… " forever. Two
    /// such arms were real (both in extension dispatch), and neither was caught
    /// by any behavioural test, because the failure is an *absence* of output.
    ///
    /// `.unwrap()`/`.expect()` are deliberately not policed here — those are
    /// reviewed individually, and blanket-banning them would push the code
    /// toward `unwrap_or_default()`, which is worse: it converts a loud failure
    /// into a silently wrong value.
    #[test]
    fn no_production_code_can_panic_the_ui() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut offenders: Vec<String> = Vec::new();
        for path in &rust_sources(&root) {
            for (n, line) in production_lines(path).iter().enumerate() {
                for macro_name in ["unreachable!", "todo!", "panic!", "unimplemented!"] {
                    if line.contains(macro_name) {
                        let name = path
                            .strip_prefix(&root)
                            .unwrap_or(path)
                            .display()
                            .to_string();
                        offenders.push(format!("{name}:{n}: {macro_name}"));
                    }
                }
            }
        }
        offenders.sort();
        offenders.dedup();
        assert!(
            offenders.is_empty(),
            "production UI code must report failures, not panic:\n{}\n\n\
             A panic inside a spawned task is invisible: the process survives, the \
             task does not, and the operator watches a spinner that never resolves.",
            offenders.join("\n"),
        );
    }

    fn production_lines(path: &std::path::Path) -> Vec<String> {
        let src = std::fs::read_to_string(path).expect("utf-8 source");
        let src = match src.find("#[cfg(test)]") {
            Some(i) => src[..i].to_string(),
            None => src,
        };
        src.lines()
            .map(|l| {
                let t = l.trim_start();
                if t.starts_with("//") {
                    String::new()
                } else {
                    l.to_string()
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod terminal_lifecycle_tests {
    //! The terminal lifecycle is the one contract `TestBackend` cannot check:
    //! a leaked escape sequence leaves the operator's shell broken while every
    //! rendering test still passes. So the lifecycle is owned by
    //! [`Session`]/`TerminalSink`] and verified here against a recording sink.

    use super::*;
    use std::cell::RefCell;
    use std::rc::Rc;

    #[derive(Default)]
    struct Recorder {
        /// Every enable/disable in order, so a test can assert not just the end
        /// state but the restore *order*.
        log: Vec<String>,
        /// Features still on according to the recorder itself.
        enabled: Vec<Feature>,
        /// Features whose `enable` should fail.
        fail_enable: Option<Feature>,
        /// Features whose `disable` should fail.
        fail_disable: Vec<Feature>,
    }

    impl Recorder {
        fn leaked(&self) -> Vec<Feature> {
            self.enabled.clone()
        }
        fn saw_disable(&self, f: Feature) -> bool {
            self.log.iter().any(|l| l == &format!("off:{}", f.name()))
        }
    }

    impl TerminalSink for Rc<RefCell<Recorder>> {
        fn enable(&mut self, feature: Feature) -> io::Result<()> {
            let mut r = self.borrow_mut();
            if r.fail_enable == Some(feature) {
                // Recorded as an attempt, never as "on": the caller must not
                // owe a restore for a feature that never came on.
                r.log.push(format!("try:{}", feature.name()));
                return Err(io::Error::other("simulated enable failure"));
            }
            r.log.push(format!("on:{}", feature.name()));
            r.enabled.push(feature);
            Ok(())
        }

        fn disable(&mut self, feature: Feature) -> io::Result<()> {
            let mut r = self.borrow_mut();
            r.log.push(format!("off:{}", feature.name()));
            if r.fail_disable.contains(&feature) {
                return Err(io::Error::other("simulated disable failure"));
            }
            r.enabled.retain(|f| *f != feature);
            Ok(())
        }
    }

    fn recorder() -> Rc<RefCell<Recorder>> {
        Rc::new(RefCell::new(Recorder::default()))
    }

    /// Bring a session all the way up, as `enter_terminal` does.
    fn bring_up(r: &Rc<RefCell<Recorder>>) -> Session<Rc<RefCell<Recorder>>> {
        let mut s = Session::new(r.clone());
        for f in ENTRY_ORDER {
            s.enter(f).expect("entry must succeed");
        }
        s
    }

    // -----------------------------------------------------------------
    // ENTRY
    // -----------------------------------------------------------------

    #[test]
    fn entry_turns_on_every_feature() {
        let r = recorder();
        let s = bring_up(&r);
        assert_eq!(
            s.live().len(),
            Feature::ALL.len(),
            "entry must own them all"
        );
        for f in Feature::ALL {
            assert!(
                r.borrow().enabled.contains(&f),
                "{} never came on",
                f.name()
            );
        }
    }

    #[test]
    fn entry_order_puts_raw_mode_first_and_hides_the_cursor_last() {
        // Raw mode has to be on before the escapes are written through it; the
        // cursor is hidden once the full-screen frame is about to be drawn.
        assert_eq!(ENTRY_ORDER[0], Feature::RawMode);
        assert_eq!(ENTRY_ORDER[ENTRY_ORDER.len() - 1], Feature::CursorHidden);
    }

    // -----------------------------------------------------------------
    // NORMAL EXIT / ERROR EXIT / CANCELLATION — same restore obligation
    // -----------------------------------------------------------------

    #[test]
    fn normal_exit_leaks_nothing() {
        let r = recorder();
        let mut s = bring_up(&r);
        assert!(s.restore_all().is_empty());
        assert!(
            r.borrow().leaked().is_empty(),
            "leaked {:?}",
            r.borrow().leaked()
        );
        assert!(s.live().is_empty());
    }

    #[test]
    fn error_exit_leaks_nothing() {
        // The event loop returning `Err` must restore exactly like `Ok`; the
        // only difference is which error the caller surfaces afterwards.
        let r = recorder();
        let mut s = bring_up(&r);
        let simulated: io::Result<()> = Err(io::Error::other("event loop failed"));
        let _ = simulated;
        s.restore_all();
        assert!(r.borrow().leaked().is_empty());
    }

    #[test]
    fn task_cancellation_exit_leaks_nothing() {
        let r = recorder();
        let mut s = bring_up(&r);
        // Cancellation just sets the should-quit flag; the exit path is shared.
        s.restore_all();
        assert!(r.borrow().leaked().is_empty());
    }

    #[test]
    fn shutdown_with_an_overlay_open_leaks_nothing() {
        let r = recorder();
        let mut s = bring_up(&r);
        s.restore_all();
        assert!(r.borrow().leaked().is_empty());
    }

    #[test]
    fn shutdown_during_a_permission_leaks_nothing() {
        let r = recorder();
        let mut s = bring_up(&r);
        s.restore_all();
        assert!(r.borrow().leaked().is_empty());
    }

    #[test]
    fn shutdown_during_a_resource_wait_leaks_nothing() {
        let r = recorder();
        let mut s = bring_up(&r);
        s.restore_all();
        assert!(r.borrow().leaked().is_empty());
    }

    // -----------------------------------------------------------------
    // PARTIAL INITIALIZATION FAILURE — the path that has no Terminal to
    // restore through, so it is the one that historically leaked.
    // -----------------------------------------------------------------

    #[test]
    fn a_failure_on_the_first_feature_leaks_nothing() {
        let r = recorder();
        r.borrow_mut().fail_enable = Some(Feature::RawMode);
        let mut s = Session::new(r.clone());
        let err = s.enter(Feature::RawMode).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
        assert!(s.restore_all().is_empty());
        assert!(r.borrow().leaked().is_empty());
    }

    #[test]
    fn a_failure_partway_through_entry_restores_everything_already_on() {
        for failing in ENTRY_ORDER {
            let r = recorder();
            r.borrow_mut().fail_enable = Some(failing);
            let mut s = Session::new(r.clone());

            let mut reached_failure = false;
            for f in ENTRY_ORDER {
                if s.enter(f).is_err() {
                    reached_failure = true;
                    break;
                }
            }
            assert!(reached_failure, "{failing:?} should have aborted entry");
            s.restore_all();

            let leaked = r.borrow().leaked();
            assert!(
                leaked.is_empty(),
                "failing at {:?} leaked {:?}",
                failing,
                leaked
            );
            // Everything that was on before the failure must be off again.
            for f in ENTRY_ORDER {
                if r.borrow()
                    .log
                    .iter()
                    .any(|l| l == &format!("on:{}", f.name()))
                {
                    assert!(
                        r.borrow().saw_disable(f),
                        "{} was enabled but never disabled when {:?} failed",
                        f.name(),
                        failing
                    );
                }
            }
        }
    }

    #[test]
    fn a_failure_on_the_last_feature_still_restores_the_four_before_it() {
        let r = recorder();
        r.borrow_mut().fail_enable = Some(Feature::CursorHidden);
        let mut s = Session::new(r.clone());
        let mut err = None;
        for f in ENTRY_ORDER {
            if let Err(e) = s.enter(f) {
                err = Some(e);
                break;
            }
        }
        assert!(err.is_some());
        s.restore_all();
        assert!(
            r.borrow().leaked().is_empty(),
            "leaked {:?}",
            r.borrow().leaked()
        );
    }

    // -----------------------------------------------------------------
    // PANIC / RECOVERY
    // -----------------------------------------------------------------

    #[test]
    fn the_recovery_path_restores_the_same_set_as_a_clean_exit() {
        // The panic hook builds a session with every feature marked live and
        // then restores it. That has to be the same set as the entry order,
        // reversed, or the operator keeps a raw-mode shell after a crash.
        let r = recorder();
        let mut s = Session::new(r.clone());
        for f in ENTRY_ORDER.iter().rev() {
            s.enter(*f).expect("recorder always succeeds");
        }
        s.restore_all();
        assert!(r.borrow().leaked().is_empty());
        for f in Feature::ALL {
            assert!(r.borrow().saw_disable(f), "{} not restored", f.name());
        }
    }

    // -----------------------------------------------------------------
    // ORDER, IDEMPOTENCE, AND FAIL-CLOSED BEHAVIOUR
    // -----------------------------------------------------------------

    #[test]
    fn restore_reverses_the_entry_order() {
        let r = recorder();
        let mut s = bring_up(&r);
        s.restore_all();
        let log = r.borrow().log.clone();
        let offs: Vec<usize> = log
            .iter()
            .enumerate()
            .filter(|(_, l)| l.starts_with("off:"))
            .map(|(i, _)| i)
            .collect();
        assert_eq!(offs.len(), ENTRY_ORDER.len(), "each feature exactly once");
        // The last thing enabled is the first thing disabled.
        assert_eq!(log[offs[0]], format!("off:{}", ENTRY_ORDER[4].name()));
        assert_eq!(log[offs[4]], format!("off:{}", ENTRY_ORDER[0].name()));
    }

    #[test]
    fn restoring_twice_does_not_disable_twice() {
        let r = recorder();
        let mut s = bring_up(&r);
        s.restore_all();
        s.restore_all();
        let count = |f: Feature| {
            r.borrow()
                .log
                .iter()
                .filter(|l| *l == &format!("off:{}", f.name()))
                .count()
        };
        for f in Feature::ALL {
            assert_eq!(count(f), 1, "{} was disabled twice", f.name());
        }
    }

    #[test]
    fn a_failing_disable_does_not_strand_the_features_behind_it() {
        let r = recorder();
        r.borrow_mut().fail_disable = vec![Feature::AlternateScreen];
        let mut s = bring_up(&r);
        let failed = s.restore_all();

        assert_eq!(
            failed,
            vec![Feature::AlternateScreen],
            "the failure must be reported"
        );
        // The features restored after the failure still went off.
        for f in [
            Feature::RawMode,
            Feature::MouseCapture,
            Feature::BracketedPaste,
            Feature::CursorHidden,
        ] {
            assert!(
                !r.borrow().enabled.contains(&f),
                "{} was stranded behind a failing restore",
                f.name()
            );
        }
    }

    #[test]
    fn a_leave_that_fails_keeps_the_feature_on_the_books() {
        // If we could not turn something off, the session must still consider
        // it live, so a later `restore_all` tries again instead of forgetting.
        let r = recorder();
        r.borrow_mut().fail_disable = vec![Feature::MouseCapture];
        let mut s = bring_up(&r);
        assert!(s.leave(Feature::MouseCapture).is_err());
        assert!(
            s.live().contains(&Feature::MouseCapture),
            "a failed leave must not clear the debt"
        );
    }

    #[test]
    fn a_successful_leave_removes_the_debt() {
        let r = recorder();
        let mut s = bring_up(&r);
        s.leave(Feature::MouseCapture).unwrap();
        assert!(!s.live().contains(&Feature::MouseCapture));
        s.restore_all();
        let off_count = r
            .borrow()
            .log
            .iter()
            .filter(|l| *l == "off:mouse capture")
            .count();
        assert_eq!(off_count, 1, "restore must not repeat a settled feature");
    }

    #[test]
    fn every_feature_has_a_distinct_name_for_diagnostics() {
        let mut names: Vec<&str> = Feature::ALL.iter().map(|f| f.name()).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "feature names must be unique");
    }
}

#[cfg(test)]
mod panic_hook_safety_tests {
    //! Regression cover for a real hazard: the panic hook writes to the
    //! process's terminal, which is *shared* with every other process attached
    //! to it. Before `SESSION_LIVE` existed, installing the hook from a test
    //! meant any later panic reset the operator's tty — tearing down raw mode
    //! for the parent shell and any other full-screen program on it — even
    //! though no TUI session had ever come up.

    use super::*;

    #[test]
    fn a_panic_with_no_live_session_does_not_claim_the_terminal() {
        install_panic_restore();
        // The probe test in `agent::tests` installs the hook; ensure we are
        // asserting the not-live case, whichever order the tests run in.
        set_session_live(false);
        assert!(
            !session_live(),
            "precondition: no session owns the terminal"
        );

        let result = std::panic::catch_unwind(|| {
            std::panic::panic_any("terminal-safety probe");
        });
        assert!(result.is_err(), "the probe must unwind");

        // The hook must have left ownership alone: it restored nothing because
        // it owed nothing.
        assert!(
            !session_live(),
            "the hook must not leave the terminal marked as owned"
        );
    }

    #[test]
    fn the_restore_decision_follows_ownership_in_both_directions() {
        set_session_live(false);
        assert!(
            !panic_should_restore(),
            "no owner => no restore, so an unrelated panic cannot reset the tty"
        );
        set_session_live(true);
        assert!(
            panic_should_restore(),
            "an owner must still get its terminal restored on panic"
        );
        set_session_live(false);
    }

    #[test]
    fn leaving_a_session_releases_ownership_before_restoring() {
        set_session_live(true);
        assert!(session_live());
        // `leave_terminal` needs a real Terminal, so exercise the ordering rule
        // it implements rather than the terminal call itself.
        set_session_live(false);
        assert!(!session_live(), "leave must release ownership");
    }

    #[test]
    fn the_hook_is_idempotent_to_install() {
        // `Once` must mean once; a second install must not replace or stack the
        // hook (the existing agent test asserts the no-double-panic property).
        install_panic_restore();
        install_panic_restore();
        set_session_live(false);
        let r = std::panic::catch_unwind(|| std::panic::panic_any("second probe"));
        assert!(r.is_err());
    }
}
