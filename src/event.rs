use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const DEPTH_WARNING_THRESHOLD: usize = 10_000;

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    PtyOutput { session_id: usize, data: Vec<u8> },
    Tick,
    SessionExited { session_id: usize, code: i32 },
    Resize(u16, u16),
}

pub struct MonitoredSender {
    inner: mpsc::Sender<Event>,
    depth: Arc<AtomicUsize>,
}

impl MonitoredSender {
    #[allow(dead_code)] // used by integration tests
    pub fn wrap(sender: mpsc::Sender<Event>) -> Self {
        Self {
            inner: sender,
            depth: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn send(&self, event: Event) -> Result<(), mpsc::SendError<Event>> {
        self.inner.send(event)?;
        // `fetch_add` wraps per atomic spec; the `saturating_add` on
        // the returned old value prevents a debug-mode panic on the
        // `old + 1` arithmetic in the (theoretically impossible but
        // seen in the wild — see below) case where the counter has
        // underflowed past 0 into `usize::MAX`.
        //
        // This counter is **observability only** (threshold warning);
        // the channel itself is unbounded mpsc and doesn't rely on
        // the count being correct. If the arithmetic goes sideways
        // the worst case is a missed warning, not data loss.
        //
        // Root cause of the observed underflow is unclear —
        // decrement sites (`next_timeout`, `try_next`) both gate
        // on a successful `recv`, so they should match increment
        // sites 1:1. Possibilities: (a) a cloned sender outlived
        // the receiver, letting a late `send` increment after the
        // receiver side already processed all events; (b) signed
        // / unsigned confusion somewhere upstream. Since the
        // counter is advisory, `saturating_add` + `saturating_sub`
        // make the bug non-fatal while we investigate.
        let old = self.depth.fetch_add(1, Ordering::Relaxed);
        let d = old.saturating_add(1);
        if d == DEPTH_WARNING_THRESHOLD {
            log::warn!("event channel depth reached {d} — possible backpressure");
        }
        Ok(())
    }

    pub fn is_err_send(&self, event: Event) -> bool {
        self.send(event).is_err()
    }
}

impl Clone for MonitoredSender {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            depth: Arc::clone(&self.depth),
        }
    }
}

pub struct EventCollector {
    rx: mpsc::Receiver<Event>,
    tx: MonitoredSender,
    depth: Arc<AtomicUsize>,
}

impl EventCollector {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let depth = Arc::new(AtomicUsize::new(0));
        let monitored_tx = MonitoredSender {
            inner: tx,
            depth: Arc::clone(&depth),
        };
        let key_tx = monitored_tx.clone();

        thread::spawn(move || {
            loop {
                if event::poll(tick_rate).unwrap_or(false) {
                    match event::read() {
                        Ok(CrosstermEvent::Key(key)) => {
                            if key_tx.is_err_send(Event::Key(key)) {
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Mouse(mouse)) => {
                            if key_tx.is_err_send(Event::Mouse(mouse)) {
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Resize(w, h)) => {
                            if key_tx.is_err_send(Event::Resize(w, h)) {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        Self {
            rx,
            tx: monitored_tx,
            depth,
        }
    }

    pub fn sender(&self) -> MonitoredSender {
        self.tx.clone()
    }

    pub fn next_timeout(&self, timeout: Duration) -> Option<Event> {
        let event = self.rx.recv_timeout(timeout).ok()?;
        saturating_dec(&self.depth);
        Some(event)
    }

    pub fn try_next(&self) -> Option<Event> {
        let event = self.rx.try_recv().ok()?;
        saturating_dec(&self.depth);
        Some(event)
    }
}

/// Decrement the depth counter without wrapping past 0. Uses a
/// compare-and-swap loop so concurrent senders and the single
/// receiver can't race the counter into `usize::MAX`. This is
/// advisory observability — we care about keeping it in a sensible
/// range, not strict accuracy.
fn saturating_dec(counter: &AtomicUsize) {
    let mut current = counter.load(Ordering::Relaxed);
    loop {
        if current == 0 {
            return;
        }
        match counter.compare_exchange_weak(
            current,
            current - 1,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return,
            Err(actual) => current = actual,
        }
    }
}
