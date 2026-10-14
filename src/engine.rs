use crate::cli::Config;
use crate::matcher::Matcher;
use crate::report::{Console, Hit, Stats};
use crate::scan::{self, Outcome};
use std::fs::File;
use std::io;
use std::sync::mpsc::Sender;

struct Suffix {
    lower: String,
    #[cfg(windows)]
    units: Vec<u16>,
}

impl Suffix {
    fn new(extension: &str) -> Suffix {
        let lower = format!(".{extension}").to_lowercase();
        Suffix {
            #[cfg(windows)]
            units: lower.encode_utf16().collect(),
            lower,
        }
    }

    fn ascii(&self) -> bool {
        self.lower.is_ascii()
    }
}

pub struct Engine {
    pub cfg: Config,
    pub matcher: Matcher,
    pub stats: Stats,
    pub console: &'static Console,
    hits: Sender<Hit>,
    suffixes: Vec<Suffix>,
}

impl Engine {
    pub fn new(
        cfg: Config,
        matcher: Matcher,
        console: &'static Console,
        hits: Sender<Hit>,
    ) -> Engine {
        let suffixes = cfg.extensions.iter().map(|e| Suffix::new(e)).collect();
        Engine {
            cfg,
            matcher,
            stats: Stats::default(),
            console,
            hits,
            suffixes,
        }
    }

    #[cfg(not(windows))]
    pub fn wants(&self, name: &[u8]) -> bool {
        self.suffixes.iter().any(|s| {
            let n = s.lower.len();
            if name.len() >= n && name[name.len() - n..].is_ascii() && s.ascii() {
                return name[name.len() - n..].eq_ignore_ascii_case(s.lower.as_bytes());
            }
            !name.is_ascii() && {
                let tail = &name[name.len().saturating_sub(n * 2)..];
                String::from_utf8_lossy(tail)
                    .to_lowercase()
                    .ends_with(&s.lower)
            }
        })
    }

    #[cfg(windows)]
    pub fn wants_wide(&self, name: &[u16]) -> bool {
        self.suffixes.iter().any(|s| {
            let n = s.units.len();
            if name.len() >= n && s.ascii() {
                let tail = &name[name.len() - n..];
                if tail.iter().all(|&u| u < 0x80) {
                    return tail
                        .iter()
                        .zip(&s.units)
                        .all(|(&a, &b)| (a as u8).eq_ignore_ascii_case(&(b as u8)));
                }
            }
            name.iter().any(|&u| u >= 0x80) && {
                let tail = &name[name.len().saturating_sub(n * 2)..];
                String::from_utf16_lossy(tail)
                    .to_lowercase()
                    .ends_with(&s.lower)
            }
        })
    }

    pub fn process(
        &self,
        opened: io::Result<File>,
        listed_size: Option<u64>,
        path: impl FnOnce() -> String,
    ) {
        let Ok(file) = opened else {
            self.stats.failed.inc();
            return;
        };
        let limit = self.cfg.max_file_bytes;
        if listed_size.is_some_and(|size| size > limit) {
            match file_size(&file) {
                Ok(size) if size > limit => {
                    self.stats.too_large.inc();
                    return;
                }
                Ok(_) => {}
                Err(_) => {
                    self.stats.failed.inc();
                    return;
                }
            }
        }
        match scan::scan(&file, limit, listed_size, &self.matcher) {
            Ok(Outcome::TooLarge) => self.stats.too_large.inc(),
            Ok(Outcome::Scanned { bytes, found }) => {
                self.stats.scanned.inc();
                self.stats.bytes.add(bytes);
                if let Some(found) = found {
                    self.stats.matched.inc();
                    let _ = self.hits.send(Hit {
                        path: path(),
                        found,
                    });
                }
            }
            Err(_) => self.stats.failed.inc(),
        }
    }
}

#[cfg(windows)]
fn file_size(file: &File) -> io::Result<u64> {
    crate::windows::ffi::file_size(file)
}

#[cfg(not(windows))]
fn file_size(file: &File) -> io::Result<u64> {
    Ok(file.metadata()?.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli;

    fn engine(extensions: &[&str]) -> Engine {
        let mut cfg = cli::Config::for_tests();
        cfg.extensions = extensions.iter().map(|e| e.to_string()).collect();
        let (tx, _rx) = std::sync::mpsc::channel();
        let console: &'static Console = Box::leak(Box::new(Console::new()));
        Engine::new(cfg, Matcher::new("x").unwrap(), console, tx)
    }

    fn check(e: &Engine, name: &str) -> bool {
        #[cfg(windows)]
        {
            e.wants_wide(&name.encode_utf16().collect::<Vec<_>>())
        }
        #[cfg(not(windows))]
        {
            e.wants(name.as_bytes())
        }
    }

    #[test]
    fn extension_matching() {
        let e = engine(&["txt", "טקסט"]);
        assert!(check(&e, "a.txt"));
        assert!(check(&e, "A.TXT"));
        assert!(check(&e, ".txt"));
        assert!(check(&e, "שלום.TxT"));
        assert!(check(&e, "קובץ.טקסט"));
        assert!(!check(&e, "a.txt.bak"));
        assert!(!check(&e, "atxt"));
        assert!(!check(&e, "a.tx"));
        assert!(!check(&e, "טקסט"));
    }
}
