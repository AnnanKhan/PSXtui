//! Entry point and event loop.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{Event, KeyEventKind};
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

#[tokio::main]
async fn main() -> Result<()> {
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

    let input_rx = spawn_input_reader();
    let result = run(&mut app, &mut ev_rx, input_rx).await;

    ratatui::restore();
    result
}

async fn run(
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<DataEvent>,
    mut input_rx: mpsc::UnboundedReceiver<Event>,
) -> Result<()> {
    let mut terminal = ratatui::init();
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
                match event {
                    Event::Key(key) if key.kind == KeyEventKind::Press => app.on_key(key),
                    Event::Resize(_, _) => {}
                    _ => continue,
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
                if !app.is_busy() {
                    continue;
                }
                app.tick();
            }
        }

        if app.should_quit {
            return Ok(());
        }
        terminal.draw(|f| ui::draw(f, app))?;
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
