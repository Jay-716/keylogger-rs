use input::{Libinput, LibinputInterface, Event};
use input::event::keyboard::{KeyboardEventTrait, KeyState};
use libc::{O_RDONLY, O_RDWR, O_WRONLY};
use std::fs::{File, OpenOptions};
use std::os::unix::{fs::OpenOptionsExt, io::OwnedFd};
use std::path::Path;
use std::thread;
use std::error::Error;
use std::sync::mpsc::sync_channel;
use signal_hook::{consts::{SIGTERM, SIGINT}, iterator::Signals};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use sqlite::{OpenFlags, Value, State};
use std::time::{SystemTime, UNIX_EPOCH};
use xdg::BaseDirectories;

struct Interface;

impl LibinputInterface for Interface {
    fn open_restricted(&mut self, path: &Path, flags: i32) -> Result<OwnedFd, i32> {
        OpenOptions::new()
            .custom_flags(flags)
            .read((flags & O_RDONLY != 0) | (flags & O_RDWR != 0))
            .write((flags & O_WRONLY != 0) | (flags & O_RDWR != 0))
            .open(path)
            .map(|file| file.into())
            .map_err(|err| err.raw_os_error().unwrap())
    }
    fn close_restricted(&mut self, fd: OwnedFd) {
        drop(File::from(fd));
    }
}

#[derive(Debug)]
pub struct KeyEvent {
    time_sec: u32,
    time_usec: u64,
    key_code: u32,
    created_at: u64,
}

struct KeySaver {
    buf: Vec<KeyEvent>,
    conn: sqlite::Connection,
}

impl KeySaver {
    fn new(path: &str) -> Self {
        let flags = OpenFlags::new()
            .with_create()
            .with_read_write();
        let conn = sqlite::Connection::open_with_flags(path, flags).unwrap();

        conn.execute("PRAGMA journal_mode=WAL;").unwrap();
        conn.execute("
CREATE TABLE IF NOT EXISTS keylogs (
    id INTEGER PRIMARY KEY,
    time_sec INTEGER NOT NULL,
    time_usec INTEGER NOT NULL,
    key_code INTEGER NOT NULL,
    created_at INTEGER DEFAULT CURRENT_TIMESTAMP
);
            ").unwrap();

        Self {
            buf: Vec::new(),
            conn,
        }
    }

    fn buf_len(&self) -> usize {
        self.buf.len()
    }

    fn add(&mut self, key_event: KeyEvent) {
        self.buf.push(key_event);
    }

    fn save(&mut self) -> Result<(), Box<dyn Error>> {
        println!("Saving {} records", self.buf.len());
        if self.buf.is_empty() {
            return Ok(());
        }

        let mut sql = String::from("INSERT INTO keylogs (time_sec, time_usec, key_code, created_at) VALUES ");
        let value_placeholder = "(?, ?, ?, ?)";
        let placeholders: Vec<&str> = (0..self.buf.len()).map(|_| value_placeholder).collect();
        sql.push_str(&placeholders.join(", "));
        sql.push(';');

        let mut row_values = Vec::with_capacity(self.buf.len());
        for ev in self.buf.drain(..) {
            row_values.push((ev.time_sec as i64).into());
            row_values.push((ev.time_usec as i64).into());
            row_values.push((ev.key_code as i64).into());
            row_values.push((ev.created_at as i64).into());
        }

        let mut stmt = self.conn.prepare(sql)?;
        stmt.bind::<&[Value]>(&row_values[..])?;
        while let Ok(State::Row) = stmt.next() {}

        Ok(())
    }
}

fn now() -> u64 {
    let start = SystemTime::now();
    let now = start
        .duration_since(UNIX_EPOCH)
        .expect("time should go forward");
    now.as_secs()
}

fn main() -> Result<(), Box<dyn Error>> {
    let running = Arc::new(Mutex::new(true));
    let running1 = Arc::clone(&running);
    let mut signals = Signals::new([SIGTERM, SIGINT])?;
    let signal_handle = thread::spawn(move || {
        for sig in signals.forever() {
            println!("Caught signal {sig}, exiting...");
            *running1.lock().unwrap() = false;
            break;
        }
    });

    let (key_tx, key_rx) = sync_channel(1000);
    let running2 = Arc::clone(&running);
    let input_handle = thread::spawn(move || {
        let mut input = Libinput::new_with_udev(Interface);
        input.udev_assign_seat("seat0").unwrap();
        loop {
            input.dispatch().unwrap();
            for event in &mut input {
                if let Event::Keyboard(event) = event {
                    if let KeyState::Pressed = event.key_state() {
                        let key_event = KeyEvent {
                            time_sec: event.time(),
                            time_usec: event.time_usec(),
                            key_code: event.key(),
                            created_at: now(),
                        };
                        key_tx.send(key_event).unwrap();
                    }
                }
            }

            if !*running2.lock().unwrap() {
                break;
            }
        }
    });

    let running3 = Arc::clone(&running);
    let save_handle = thread::spawn(move || {
        let dir = BaseDirectories::new();
        let file = dir.place_data_file("keylogger-rs/keylogger.db").unwrap();
        let file = file.to_str().unwrap();

        let mut key_saver = KeySaver::new(file);
        let mut last_save = 0;

        loop {
            let result = key_rx.recv_timeout(Duration::from_millis(500));
            if let Ok(event) = result {
                key_saver.add(event);
            }
            let now = now();
            if key_saver.buf_len() >= 100 || now - last_save > 60 {
                let _ = key_saver.save();
                last_save = now;
            }

            if !*running3.lock().unwrap() {
                let _ = key_saver.save();
                break;
            }
        }
    });

    signal_handle.join().unwrap();
    input_handle.join().unwrap();
    save_handle.join().unwrap();
    Ok(())
}

