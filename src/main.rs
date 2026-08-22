//! Entry point and event loop.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind, MouseEventKind,
};
use crossterm::execute;
use tokio::sync::mpsc;

use psxtui::app::{App, DataEvent, DataRequest};
use psxtui::cache::Store;
use psxtui::data::{BACKFILL_DAYS, Worker};
use psxtui::psx::PsxClient;
use psxtui::ui;

/// How often the market board is refreshed while the app is open.
const AUTO_REFRESH: Duration = Duration::from_secs(60);

/// Busy-spinner frame rate.
const SPINNER_TICK: Duration = Duration::from_millis(110);

/// What `--help` prints. The whole command-line surface is two flags: the app
/// is driven from inside itself, and `?` is the real help.
///
/// The cache path is resolved at run time rather than written in: it differs on
/// every platform, and a help text that names the wrong directory is worse than
/// one that names none.
fn usage() -> String {
    let db = psxtui::cache::default_db_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "your platform data directory".into());
    let themes = psxtui::ui::theme::THEMES
        .iter()
        .map(|t| t.key)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "\
psxtui — a terminal client for Pakistan Stock Exchange market data

Usage: psxtui [OPTIONS]

Options:
  -h, --help       show this message
  -V, --version    show the version

Environment:
  PSXTUI_THEME=<name>   start in a theme: {themes}
  PSXTUI_GRAPHICS=off   draw charts with glyphs even on a terminal that could
                        render them as images (=kitty forces the other way)
  PSXTUI_CELL=9x18      cell size in pixels, if the terminal misreports it
  PSXTUI_MARKER=block   draw charts with half-block glyphs instead of braille,
                        for fonts that have no braille (some Windows consoles)

Everything else happens inside the app: press ? for keys, T for the next
theme, q to quit.
Cached data lives in {db}
"
    )
}

/// Put the Windows console into UTF-8.
///
/// A console left on a regional code page renders every box-drawing and braille
/// glyph as mojibake. Output only: crossterm reads input through the wide-char
/// API, so the input code page buys nothing and changing it could surprise
/// whatever runs in the same console afterwards.
#[cfg(windows)]
fn use_utf8_console() {
    const CP_UTF8: u32 = 65001;
    // SAFETY: a plain console-attribute setter with no memory involved; it
    // fails harmlessly (returning 0) when there is no console attached.
    unsafe {
        windows_sys::Win32::System::Console::SetConsoleOutputCP(CP_UTF8);
    }
}

#[cfg(not(windows))]
fn use_utf8_console() {}

#[tokio::main]
async fn main() -> Result<()> {
    use_utf8_console();

    // Anything on PATH is expected to answer --version and --help without
    // seizing the terminal, not least so an installer can verify itself.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("psxtui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                print!("{}", usage());
                return Ok(());
            }
            other => {
                eprint!("psxtui: unknown option '{other}'\n\n{}", usage());
                std::process::exit(2);
            }
        }
    }

    let store = Arc::new(Store::open_default().context("opening local cache")?);
    let client = Arc::new(PsxClient::new()?);

    let (req_tx, req_rx) = mpsc::unbounded_channel::<DataRequest>();
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<DataEvent>();

    let worker = Worker::new(client, store.clone(), ev_tx);
    worker.prime_from_cache();
    tokio::spawn(worker.run(req_rx));

    let mut app = App::new(store, req_tx);
    app.request(DataRequest::RefreshMarket);
    app.request(DataRequest::Backfill(BACKFILL_DAYS));
    app.request(DataRequest::RefreshExternal);

    let input_rx = spawn_input_reader();
    let result = run(&mut app, &mut ev_rx, input_rx).await;

    // Leave mouse reporting off on the way out: a terminal left in that mode
    // after the process exits stops responding to selection entirely. Charts
    // drawn as images have to go the same way — a placement outlives the
    // process that made it, and would otherwise sit on top of the shell.
    let _ = ui::gfx::clear_all(&mut std::io::stdout());
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    result
}

async fn run(
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<DataEvent>,
    mut input_rx: mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
    let mut terminal = ratatui::init();
    // Mouse reporting is opt-out rather than opt-in: it is what users expect,
    // and `M` turns it off when the terminal's own selection is wanted.
    let mut mouse_on = false;
    set_mouse(&mut mouse_on, app.mouse_enabled);
    let mut refresh = tokio::time::interval(AUTO_REFRESH);
    // The first tick fires immediately; the startup refresh already covers it.
    refresh.tick().await;

    // Drives the busy spinner. Ticks continuously but only forces a redraw
    // while work is in flight, so an idle app costs nothing.
    let mut spinner = tokio::time::interval(SPINNER_TICK);
    // The status bar owns the market indicator. Keep polling on the existing
    // spinner cadence so it changes at the session boundary even when the app
    // is otherwise idle, without adding another timer or network request.
    let mut last_market_state = app.market_state();

    render(&mut terminal, app)?;

    loop {
        tokio::select! {
            Some(event) = input_rx.recv() => {
                // Drain every key already buffered, then redraw once.
                //
                // Held arrow keys arrive faster than a frame can be drawn.
                // Handling one per redraw let the queue grow, so scrolling
                // lagged behind the keyboard and kept moving after the key was
                // released while the backlog drained. Applying the whole burst
                // to state before drawing keeps the cursor where the user
                // actually left it.
                let mut acted = handle_input(app, event);
                while let Ok(next) = input_rx.try_recv() {
                    acted |= handle_input(app, next);
                }
                if !acted {
                    continue;
                }
            }
            Some(ev) = ev_rx.recv() => {
                app.on_event(ev);
                // Coalesce bursts — a backfill emits a status per day and
                // redrawing each one would thrash the terminal.
                while let Ok(next) = ev_rx.try_recv() {
                    app.on_event(next);
                }
            }
            _ = refresh.tick() => {
                app.request(DataRequest::RefreshMarket);
            }
            _ = spinner.tick() => {
                // Fire any fetch whose cursor has settled, then advance the
                // spinner. An idle redraw is also needed when the regular
                // market crosses an open, break, close, or weekend boundary.
                let fired = app.poll_pending_load();
                let market_state = app.market_state();
                let market_changed = market_state != last_market_state;
                if !fired && !app.is_busy() && !market_changed {
                    continue;
                }
                last_market_state = market_state;
                app.tick();
            }
        }

        if app.should_quit {
            return Ok(());
        }
        // Follow any change to the mouse toggle before drawing.
        set_mouse(&mut mouse_on, app.mouse_enabled);
        // Resolve any pending selection once, after the whole input burst has
        // been applied — not once per key.
        app.settle_selection();
        render(&mut terminal, app)?;
    }
}

/// Draw one frame, then hand any chart images to the terminal.
///
/// The two steps are separate because ratatui owns the cell grid and knows
/// nothing about images: the escape sequences have to follow its own output for
/// the frame, or they would be overwritten by it. See [`ui::gfx`].
fn render(terminal: &mut ratatui::DefaultTerminal, app: &App) -> Result<()> {
    terminal.draw(|f| ui::draw(f, app))?;
    ui::gfx::present(&mut std::io::stdout())?;
    Ok(())
}

/// Apply one terminal event. Returns whether it warrants a redraw.
fn handle_input(app: &mut App, event: Event) -> bool {
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            app.on_key(key);
            true
        }
        Event::Mouse(ev) => {
            // Plain motion arrives continuously on some terminals and changes
            // nothing, so it must not force a redraw.
            if matches!(ev.kind, MouseEventKind::Moved | MouseEventKind::Drag(_)) {
                return false;
            }
            app.on_mouse(ev);
            true
        }
        // A resize needs a repaint even though no state changed.
        Event::Resize(_, _) => true,
        _ => false,
    }
}

/// Turn terminal mouse reporting on or off, only when it actually changes.
fn set_mouse(current: &mut bool, wanted: bool) {
    if *current == wanted {
        return;
    }
    let ok = if wanted {
        execute!(std::io::stdout(), EnableMouseCapture).is_ok()
    } else {
        execute!(std::io::stdout(), DisableMouseCapture).is_ok()
    };
    if ok {
        *current = wanted;
    }
}

/// Read terminal input on a dedicated thread.
///
/// crossterm's read is blocking, and doing it on the async runtime would stall
/// the worker's timers.
fn spawn_input_reader() -> mpsc::UnboundedReceiver<Event> {
    let (tx, rx) = mpsc::unbounded_channel();
    std::thread::spawn(move || {
        // Stops when the terminal closes or the UI drops the receiver.
        while let Ok(ev) = crossterm::event::read() {
            if tx.send(ev).is_err() {
                break;
            }
        }
    });
    rx
}
