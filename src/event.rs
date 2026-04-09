use crossterm::event::{self, Event as CrosstermEvent, KeyEvent, MouseEvent};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

#[derive(Debug)]
pub enum Event {
    Key(KeyEvent),
    Mouse(MouseEvent),
    PtyOutput {
        session_id: usize,
        #[allow(dead_code)]
        data: Vec<u8>,
    },
    Tick,
    SessionExited {
        session_id: usize,
        code: i32,
    },
    Resize(u16, u16),
}

pub struct EventCollector {
    rx: mpsc::Receiver<Event>,
    tx: mpsc::Sender<Event>,
}

impl EventCollector {
    pub fn new(tick_rate: Duration) -> Self {
        let (tx, rx) = mpsc::channel();
        let key_tx = tx.clone();

        thread::spawn(move || {
            loop {
                if event::poll(tick_rate).unwrap_or(false) {
                    match event::read() {
                        Ok(CrosstermEvent::Key(key)) => {
                            if key_tx.send(Event::Key(key)).is_err() {
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Mouse(mouse)) => {
                            if key_tx.send(Event::Mouse(mouse)).is_err() {
                                break;
                            }
                        }
                        Ok(CrosstermEvent::Resize(w, h)) => {
                            if key_tx.send(Event::Resize(w, h)).is_err() {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
        });

        Self { rx, tx }
    }

    pub fn sender(&self) -> mpsc::Sender<Event> {
        self.tx.clone()
    }

    pub fn next(&self) -> Result<Event, mpsc::RecvError> {
        self.rx.recv()
    }

    pub fn try_next(&self) -> Option<Event> {
        self.rx.try_recv().ok()
    }
}
