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
const USAGE: &str = "\
psxtui — a terminal client for Pakistan Stock Exchange market data

Usage: psxtui [OPTIONS]

Options:
  -h, --help       show this message
  -V, --version    show the version

Everything else happens inside the app: press ? for keys, q to quit.
Cached data lives in ~/.local/share/psxtui/psx.db
";

#[tokio::main]
async fn main() -> Result<()> {
    // Anything on PATH is expected to answer --version and --help without
    // seizing the terminal, not least so an installer can verify itself.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("psxtui {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                print!("{USAGE}");
                return Ok(());
            }
            other => {
                eprint!("psxtui: unknown option '{other}'\n\n{USAGE}");
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
    // after the process exits stops responding to selection entirely.
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

    terminal.draw(|f| ui::draw(f, app))?;

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
                // spinner. Skip the redraw entirely when nothing is happening.
                let fired = app.poll_pending_load();
                if !fired && !app.is_busy() {
                    continue;
                }
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
        terminal.draw(|f| ui::draw(f, app))?;
    }
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
