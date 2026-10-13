use std::io::{self, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use crate::page;

/// Quicker than a Model 33 (10 cps) so a fresh report does not hide the times,
/// slow enough that each line is obviously being typed.
const CHAR_PAUSE: Duration = Duration::from_millis(10);
const LINE_PAUSE: Duration = Duration::from_millis(40);

static STOP: AtomicBool = AtomicBool::new(false);

pub fn request_stop() {
    STOP.store(true, Ordering::Relaxed);
}

pub fn stop_requested() -> bool {
    STOP.load(Ordering::Relaxed)
}

#[derive(Clone)]
pub struct Frame {
    pub cols: usize,
    pub rows: usize,
    pub left: usize,
    pub top: usize,
    pub width: usize,
    pub max_trains: usize,
    pub double_space: bool,
}

pub struct Glass {
    pub frame: Frame,
    prev: Vec<String>,
}

impl Frame {
    /// The CRT console with Terminus Bold 32×16: 45 columns, 15 rows.
    /// Local runs use this same grid so the preview matches the TV.
    pub fn detect() -> Self {
        Self::from_size(45, 15)
    }

    pub fn from_size(cols: usize, rows: usize) -> Self {
        // Leave the last column unused so a full line does not wrap.
        let usable = cols.saturating_sub(1).max(1);
        let width = usable;
        let left = cols.saturating_sub(width) / 2;
        let top = if rows > 10 { 1 } else { 0 };
        let budget = rows.saturating_sub(top + 1).max(1);
        let (max_trains, double_space) = page::capacity(budget);
        Self {
            cols,
            rows,
            left,
            top,
            width,
            max_trains,
            double_space,
        }
    }
}

impl Glass {
    pub fn open() -> io::Result<Self> {
        let glass = Self {
            frame: Frame::detect(),
            prev: Vec::new(),
        };
        print!("\x1b[2J\x1b[H");
        io::stdout().flush()?;
        Ok(glass)
    }

    /// Clear and redraw when the terminal size changes.
    pub fn refit(&mut self) {
        let next = Frame::detect();
        let changed = next.cols != self.frame.cols
            || next.rows != self.frame.rows
            || next.width != self.frame.width
            || next.left != self.frame.left
            || next.top != self.frame.top;
        if changed {
            self.frame = next;
            self.prev.clear();
            print!("\x1b[2J\x1b[H");
            let _ = io::stdout().flush();
        }
    }

    pub fn present(&mut self, lines: &[String], body: Range<usize>, structural: bool) {
        let boot = self.prev.is_empty();
        for (index, line) in lines.iter().enumerate() {
            if self.prev.get(index) == Some(line) {
                continue;
            }
            // The first page appears at once. Later train-list changes scan
            // letter by letter. `boot` is the empty previous page.
            let type_it = !boot && structural && body.contains(&index);
            if type_it {
                self.type_line(index, line);
            } else {
                self.write_line(index, line);
            }
            if stop_requested() {
                break;
            }
        }
        if self.prev.len() > lines.len() {
            let blank = " ".repeat(self.frame.width);
            for index in lines.len()..self.prev.len() {
                self.write_line(index, &blank);
            }
        }
        self.prev = lines.to_vec();
        self.park(lines.len());
        let _ = io::stdout().flush();
    }

    pub fn shutdown(&self) {
        let row = self.frame.rows.max(1);
        print!("\x1b[0m\x1b[{row};1H\n");
        let _ = io::stdout().flush();
    }

    fn type_line(&self, index: usize, text: &str) {
        self.goto(index);
        print!("\x1b[0m\x1b[K");
        self.goto(index);
        let _ = io::stdout().flush();
        self.emit(text.trim_end(), true);
        thread::sleep(LINE_PAUSE);
    }

    fn write_line(&self, index: usize, text: &str) {
        self.goto(index);
        self.emit(text, false);
    }

    /// Non-local service abbreviations are reverse video. Escape sequences are
    /// written at once so the letter-by-letter scan does not type them out.
    fn emit(&self, text: &str, typed: bool) {
        let stopped = if let Some(span) = page::reverse_span(text) {
            let end = span.end.min(text.len());
            let (badge, rest) = text.split_at(end);
            print!("\x1b[7m");
            let stopped = self.put(badge, typed);
            print!("\x1b[27m");
            stopped || self.put(rest, typed)
        } else {
            self.put(text, typed)
        };
        if stopped {
            print!("\x1b[0m");
            let _ = io::stdout().flush();
            return;
        }
        if typed {
            print!("\x1b[27m");
        } else {
            print!("\x1b[0m\x1b[K");
        }
    }

    fn put(&self, text: &str, typed: bool) -> bool {
        if !typed {
            print!("{text}");
            return false;
        }
        for ch in text.chars() {
            if stop_requested() {
                return true;
            }
            print!("{ch}");
            let _ = io::stdout().flush();
            thread::sleep(CHAR_PAUSE);
        }
        false
    }

    fn goto(&self, index: usize) {
        let row = self.frame.top + index;
        let col = self.frame.left;
        print!("\x1b[{};{}H", row + 1, col + 1);
    }

    fn park(&self, line_count: usize) {
        let row = (self.frame.top + line_count).min(self.frame.rows.saturating_sub(1));
        print!("\x1b[{};{}H", row + 1, self.frame.left + 1);
        let _ = io::stdout().flush();
    }
}
