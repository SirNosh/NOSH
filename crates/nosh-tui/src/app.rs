//! The terminal session: `runTui` in `apps/tui/src/ui.ts` (with `index.ts` `launchTui`) as a
//! `current_thread` runtime, a `LocalSet` and one event loop.
//!
//! Specified by tui-workspace.md §1.5 (startup sequence, cleanup, handoff write), §1.6 (timers),
//! §7.7 (terminal setup and teardown: alternate screen, raw mode, mouse capture, bracketed paste,
//! keyboard enhancement flags, cursor style and OSC 12 color), §10.4 (the `select!` loop over
//! crossterm's `EventStream`, the poll timer, the 50 ms render coalescer, the spinner, completions
//! and Ctrl+C/Ctrl+Break) and CA-4 (in-process, restores the terminal before returning or unwinding);
//! tui-controller.md §14 (driver integration). Private: only `crate::run` calls it.
//!
//! Frames are drawn on demand from one deadline: input asks for the next frame slot (at most
//! [`MAX_FPS`] per second), controller notifications and poll ticks for one within
//! [`RENDER_COALESCE_MS`], resizes for one [`RESIZE_DEBOUNCE_MS`] after the last, and the spinner for
//! one every [`SPINNER_INTERVAL_MS`] while work is in flight.

use std::io::{self, IsTerminal, Stdout, Write};
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use crossterm::cursor::{SetCursorStyle, Show};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, Event,
    EventStream, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use futures_util::StreamExt;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use tokio::sync::{Notify, mpsc};
use tokio::time::{Instant, sleep, sleep_until};

use crate::config::TuiConfig;
use crate::controller::Controller;
use crate::js::{JsObject, JsValue, json_stringify};
use crate::transport::{DaemonApi, DaemonClient};
use crate::workspace::{SPINNER_INTERVAL_MS, Workspace};

/// The poll delay while `connection === 'connected'`, measured from the end of the previous poll.
pub(crate) const POLL_CONNECTED_MS: u64 = 1000;
/// The poll delay otherwise.
pub(crate) const POLL_DISCONNECTED_MS: u64 = 5000;
/// The trailing render coalescing of controller notifications.
pub(crate) const RENDER_COALESCE_MS: u64 = 50;
/// The terminal resize debounce.
pub(crate) const RESIZE_DEBOUNCE_MS: u64 = 100;
/// The frame cap (OpenTUI `maxFps`).
pub(crate) const MAX_FPS: u64 = 30;
/// `runTui`'s error when stdin or stdout is not a terminal.
pub(crate) const NEEDS_TERMINAL: &str = "NOSH TUI needs an interactive terminal";

/// `runTui(config)`: TTY check, client, `controller.initialize()`, terminal setup, the event loop
/// until quit, then teardown and one handoff write (workspace §1.5).
pub(crate) fn run(config: TuiConfig) -> Result<(), String> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(NEEDS_TERMINAL.into());
    }
    let client = Rc::new(DaemonClient::new(&config).map_err(|error| error.to_string())?);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    tokio::task::LocalSet::new().block_on(&runtime, session(config, client))
}

/// Startup after the client exists, the session, then cleanup in TS order: the handoff, the
/// controller, the client; the terminal guard restores the screen before this returns.
async fn session(config: TuiConfig, client: Rc<DaemonClient>) -> Result<(), String> {
    let controller = Controller::new(client.clone(), config.current_project_id.clone());
    if let Err(error) = controller.initialize().await {
        client.close();
        return Err(error);
    }
    let result = match TerminalGuard::enter() {
        Ok(guard) => {
            let result = event_loop(&controller).await;
            drop(guard);
            result
        }
        Err(error) => Err(error.to_string()),
    };
    if let Some(path) = config
        .handoff_path
        .as_deref()
        .filter(|path| !path.as_os_str().is_empty())
    {
        write_handoff(path, &controller.state().project_id);
    }
    controller.dispose();
    client.close();
    result
}

/// The event loop of workspace §10.4 until the workspace asks to quit or a signal arrives.
async fn event_loop(controller: &Rc<Controller>) -> Result<(), String> {
    let mut terminal =
        Terminal::new(CrosstermBackend::new(io::stdout())).map_err(|error| error.to_string())?;
    let (completions_tx, mut completions) = mpsc::unbounded_channel();
    let mut workspace = Workspace::new(controller.clone(), completions_tx);
    let changed = Rc::new(Notify::new());
    let listener = {
        let changed = changed.clone();
        controller.subscribe(Rc::new(move || changed.notify_one()))
    };
    // One bounded reconciliation page per tick; the controller owns live deltas.
    let poller = tokio::task::spawn_local({
        let (controller, changed) = (controller.clone(), changed.clone());
        async move {
            loop {
                controller.poll().await;
                changed.notify_one();
                let connected = controller.state().connection == "connected";
                sleep(Duration::from_millis(if connected {
                    POLL_CONNECTED_MS
                } else {
                    POLL_DISCONNECTED_MS
                }))
                .await;
            }
        }
    });
    let mut events = EventStream::new();
    let mut signal = std::pin::pin!(quit_signal());
    let frame = Duration::from_millis(1000 / MAX_FPS);
    let mut last_draw = Instant::now() - frame;
    let mut deadline = Some(Instant::now());
    let mut spin_at: Option<Instant> = None;
    let result = loop {
        let next_frame = (last_draw + frame).max(Instant::now());
        tokio::select! {
            event = events.next() => match event {
                Some(Ok(Event::Resize(..))) => {
                    deadline = Some(Instant::now() + Duration::from_millis(RESIZE_DEBOUNCE_MS));
                }
                Some(Ok(event)) => {
                    workspace.handle_event(&event);
                    // Lets a command spawned by this input run to its first await, so its busy
                    // state is in the next frame (TS runs that prefix synchronously).
                    tokio::task::yield_now().await;
                    deadline = Some(deadline.map_or(next_frame, |at| at.min(next_frame)));
                }
                Some(Err(error)) => break Err(error.to_string()),
                None => break Ok(()),
            },
            Some(completion) = completions.recv() => {
                workspace.handle_completion(completion);
                deadline = Some(deadline.map_or(next_frame, |at| at.min(next_frame)));
            }
            () = changed.notified() => {
                let coalesced = next_frame.max(Instant::now() + Duration::from_millis(RENDER_COALESCE_MS));
                deadline.get_or_insert(coalesced);
            }
            () = sleep_until(spin_at.unwrap_or_else(Instant::now)), if spin_at.is_some() => {
                spin_at = None;
                deadline = Some(deadline.map_or(next_frame, |at| at.min(next_frame)));
            }
            () = sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {
                deadline = None;
                last_draw = Instant::now();
                if let Err(error) = terminal.draw(|frame| workspace.draw(frame)) {
                    break Err(error.to_string());
                }
            }
            () = &mut signal => break Ok(()),
        }
        if workspace.quit_requested() {
            break Ok(());
        }
        // The spinner animates only while work is in flight.
        if !workspace.working() {
            spin_at = None;
        } else if spin_at.is_none() {
            spin_at = Some(Instant::now() + Duration::from_millis(SPINNER_INTERVAL_MS));
        }
    };
    poller.abort();
    controller.unsubscribe(listener);
    workspace.dispose();
    result
}

/// SIGINT and SIGTERM (Ctrl+C and Ctrl+Break on Windows) close the TUI; Ctrl+C in raw mode is a
/// key instead. A handler that cannot be installed never fires.
async fn quit_signal() {
    let interrupt = async {
        if tokio::signal::ctrl_c().await.is_err() {
            std::future::pending::<()>().await;
        }
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => drop(terminate.recv().await),
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(windows)]
    let terminate = async {
        match tokio::signal::windows::ctrl_break() {
            Ok(mut terminate) => drop(terminate.recv().await),
            Err(_) => std::future::pending().await,
        }
    };
    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
}

/// The terminal modes of workspace §7.7; dropping it restores the terminal, also on unwind.
struct TerminalGuard {
    keyboard: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let mut guard = TerminalGuard { keyboard: false };
        let mut stdout = io::stdout();
        execute!(
            stdout,
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            SetCursorStyle::BlinkingBlock
        )?;
        // OpenTUI requests kitty flags 1 (disambiguate) + 4 (alternate keys), so Shift+Enter and
        // Keypad Enter are distinguishable; the Windows console reports them natively.
        if terminal::supports_keyboard_enhancement().unwrap_or(false) {
            execute!(
                stdout,
                PushKeyboardEnhancementFlags(
                    KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                        | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
                )
            )?;
            guard.keyboard = true;
        }
        // OSC 12: the cursor color (terminals without it ignore the sequence).
        write!(stdout, "\x1b]12;#5fd4c4\x07")?;
        stdout.flush()?;
        Ok(guard)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let mut stdout: Stdout = io::stdout();
        if self.keyboard {
            let _ = execute!(stdout, PopKeyboardEnhancementFlags);
        }
        let _ = write!(stdout, "\x1b]112\x07");
        let _ = execute!(
            stdout,
            DisableBracketedPaste,
            DisableMouseCapture,
            SetCursorStyle::DefaultUserShape,
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

/// `recordProject()`: writes `{"currentProjectId":"<id>"}` (compact, no newline) when both a path
/// and a project exist; write errors are ignored.
pub(crate) fn write_handoff(path: &Path, project_id: &str) {
    if project_id.is_empty() {
        return;
    }
    let mut handoff = JsObject::new();
    handoff.set("currentProjectId", project_id.into());
    // Best effort: the next open falls back to the configured project.
    let _ = std::fs::write(path, json_stringify(&JsValue::Object(handoff)));
}
