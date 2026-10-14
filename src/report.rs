use crate::matcher::FileMatch;
use std::fs::File;
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub struct Hit {
    pub path: String,
    pub found: FileMatch,
}

#[derive(Default)]
pub struct Counter(AtomicU64);

impl Counter {
    pub fn add(&self, n: u64) {
        self.0.fetch_add(n, Relaxed);
    }

    pub fn inc(&self) {
        self.add(1);
    }

    pub fn get(&self) -> u64 {
        self.0.load(Relaxed)
    }
}

#[derive(Default)]
pub struct Stats {
    pub listed: Counter,
    pub scanned: Counter,
    pub bytes: Counter,
    pub matched: Counter,
    pub too_large: Counter,
    pub offline: Counter,
    pub failed: Counter,
}

pub struct Console {
    status: Mutex<bool>,
    live: bool,
}

const STATUS_WIDTH: usize = 110;

impl Console {
    pub fn new() -> Console {
        Console {
            status: Mutex::new(false),
            live: io::stderr().is_terminal(),
        }
    }

    fn clear(&self, shown: &mut MutexGuard<'_, bool>) {
        if **shown {
            eprint!("\r{:STATUS_WIDTH$}\r", "");
            **shown = false;
        }
    }

    pub fn line(&self, text: &str) {
        let mut shown = self.status.lock().unwrap_or_else(|e| e.into_inner());
        self.clear(&mut shown);
        let mut out = io::stdout().lock();
        let _ = writeln!(out, "{text}");
        let _ = out.flush();
    }

    pub fn warn(&self, text: &str) {
        self.note(&format!("[WARN] {text}"));
    }

    pub fn note(&self, text: &str) {
        let mut shown = self.status.lock().unwrap_or_else(|e| e.into_inner());
        self.clear(&mut shown);
        eprintln!("{text}");
    }

    fn status(&self, text: &str) {
        let mut shown = self.status.lock().unwrap_or_else(|e| e.into_inner());
        eprint!("\r{text:STATUS_WIDTH$}");
        *shown = true;
    }

    pub fn finish(&self) {
        let mut shown = self.status.lock().unwrap_or_else(|e| e.into_inner());
        self.clear(&mut shown);
    }
}

pub struct Totals {
    pub files: u64,
    pub lines: u64,
}

pub fn spawn_writer(
    output: &Path,
    hits: Receiver<Hit>,
    console: &'static Console,
) -> io::Result<JoinHandle<io::Result<Totals>>> {
    let mut csv = csv::Writer::from_writer(io::BufWriter::new(File::create(output)?));
    csv.write_record(["path", "encoding", "line_number", "line_preview"])?;
    csv.flush()?;

    Ok(std::thread::spawn(move || {
        let mut totals = Totals { files: 0, lines: 0 };
        for hit in hits {
            let enc = hit.found.encoding.label();
            let numbers: Vec<String> = hit.found.lines.iter().map(|l| l.line.to_string()).collect();
            console.line(&format!(
                "[HIT] {}  (encoding={enc})  lines={}",
                hit.path,
                numbers.join(",")
            ));
            for (line, number) in hit.found.lines.iter().zip(&numbers) {
                csv.write_record([hit.path.as_str(), enc, number, line.preview.as_str()])?;
            }
            csv.flush()?;
            totals.files += 1;
            totals.lines += hit.found.lines.len() as u64;
        }
        csv.flush()?;
        Ok(totals)
    }))
}

pub fn progress(stats: &Stats, console: &Console, started: Instant, stop: Receiver<()>) {
    if !console.live {
        let _ = stop.recv();
        return;
    }
    loop {
        match stop.recv_timeout(Duration::from_millis(250)) {
            Err(RecvTimeoutError::Timeout) => {}
            _ => return,
        }
        let secs = started.elapsed().as_secs_f64();
        let bytes = stats.bytes.get();
        console.status(&format!(
            "found {} | scanned {} | {} | {}/s | hits {} | {:.1}s",
            group(stats.listed.get()),
            group(stats.scanned.get()),
            human_bytes(bytes),
            human_bytes((bytes as f64 / secs.max(1e-3)) as u64),
            stats.matched.get(),
            secs,
        ));
    }
}

pub fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = n as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{n} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formatting() {
        assert_eq!(group(0), "0");
        assert_eq!(group(1234567), "1,234,567");
        assert_eq!(group(999), "999");
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(1536), "1.5 KB");
    }
}
