use crate::matcher::{FileMatch, Matcher};
use std::cell::RefCell;
use std::fs::File;
use std::io;

const WINDOW: usize = 4 << 20;
const FIRST_READ: usize = 64 << 10;
pub const ALIGN: usize = 4096;

#[derive(Default)]
struct Aligned(Vec<u8>);

impl Aligned {
    fn region(&mut self, len: usize) -> &mut [u8] {
        if self.0.len() < len + ALIGN {
            self.0 = vec![0u8; len + ALIGN];
        }
        let start = self.0.as_ptr().align_offset(ALIGN);
        &mut self.0[start..start + len]
    }
}

thread_local! {
    static BUFFER: RefCell<Aligned> = RefCell::new(Aligned::default());
}

#[cfg(windows)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buf, offset)
}

#[cfg(unix)]
fn read_at(file: &File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buf, offset)
}

#[cfg(windows)]
const HANDLE_EOF: i32 = 38;
#[cfg(not(windows))]
const HANDLE_EOF: i32 = i32::MIN;

fn read_up_to(file: &File, buf: &mut [u8], want: usize, mut offset: u64) -> io::Result<usize> {
    let mut filled = 0;
    while filled < want {
        match read_at(file, &mut buf[filled..want], offset) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) if e.raw_os_error() == Some(HANDLE_EOF) => break,
            Err(e) => return Err(e),
        }
    }
    Ok(filled.min(want))
}

pub enum Outcome {
    TooLarge,
    Scanned {
        bytes: u64,
        found: Option<FileMatch>,
    },
}

pub fn scan(
    file: &File,
    limit: u64,
    size_hint: Option<u64>,
    matcher: &Matcher,
) -> io::Result<Outcome> {
    let first = size_hint.map_or(FIRST_READ, |size| {
        ((size as usize).saturating_add(1))
            .min(WINDOW)
            .next_multiple_of(ALIGN)
    });
    BUFFER.with_borrow_mut(|buffer| {
        let keep = matcher.max_len() - 1;
        let prefix = keep.next_multiple_of(ALIGN).max(ALIGN);
        let buf = buffer.region(prefix + WINDOW);
        let mut offset = 0u64;
        let mut carry = 0usize;
        loop {
            let want = if offset == 0 { first } else { WINDOW };
            let n = read_up_to(file, &mut buf[prefix..], want, offset)?;
            offset += n as u64;
            if offset > limit {
                return Ok(Outcome::TooLarge);
            }
            let end_of_file = n < want;
            let window = &buf[prefix - carry..prefix + n];
            if matcher.first_candidate(window).is_some() {
                if end_of_file && offset == n as u64 {
                    return Ok(Outcome::Scanned {
                        bytes: offset,
                        found: matcher.scan(window),
                    });
                }
                return read_whole(file, limit, matcher);
            }
            if end_of_file {
                return Ok(Outcome::Scanned {
                    bytes: offset,
                    found: None,
                });
            }
            carry = keep.min(window.len());
            buf.copy_within(prefix + n - carry..prefix + n, prefix - carry);
        }
    })
}

fn read_whole(file: &File, limit: u64, matcher: &Matcher) -> io::Result<Outcome> {
    let mut data = Vec::new();
    loop {
        let start = data.len();
        data.resize(start + WINDOW, 0);
        let n = read_up_to(file, &mut data[start..], WINDOW, start as u64)?;
        data.truncate(start + n);
        if data.len() as u64 > limit {
            return Ok(Outcome::TooLarge);
        }
        if n < WINDOW {
            return Ok(Outcome::Scanned {
                bytes: data.len() as u64,
                found: matcher.scan(&data),
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, data: &[u8]) -> (std::path::PathBuf, File) {
        let dir = std::env::temp_dir().join(format!("stringfinder-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        File::create(&path).unwrap().write_all(data).unwrap();
        let file = File::open(&path).unwrap();
        (path, file)
    }

    fn found(outcome: Outcome) -> Option<FileMatch> {
        match outcome {
            Outcome::Scanned { found, .. } => found,
            Outcome::TooLarge => panic!("unexpected size limit"),
        }
    }

    #[test]
    fn hit_straddling_window_boundary() {
        let target = "boundary-needle";
        let mut data = vec![b'.'; WINDOW * 2 + 17];
        let at = WINDOW - 5;
        data[at..at + target.len()].copy_from_slice(target.as_bytes());
        data[10] = b'\n';
        let (path, file) = temp_file("straddle.txt", &data);
        let m = Matcher::new(target).unwrap();
        let hit =
            found(scan(&file, u64::MAX, None, &m).unwrap()).expect("match across the window edge");
        assert_eq!(hit.lines[0].line, 2);
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn large_file_without_hit_reads_everything() {
        let data = vec![b'z'; WINDOW * 3 + 1];
        let (path, file) = temp_file("large.txt", &data);
        let m = Matcher::new("nope").unwrap();
        match scan(&file, u64::MAX, None, &m).unwrap() {
            Outcome::Scanned { bytes, found } => {
                assert!(found.is_none());
                assert_eq!(bytes, data.len() as u64);
            }
            Outcome::TooLarge => panic!("unexpected size limit"),
        }
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn small_empty_and_limited_files() {
        let (path, file) = temp_file("small.txt", b"one\ntwo needle\n");
        let m = Matcher::new("needle").unwrap();
        assert_eq!(
            found(scan(&file, 15, Some(15), &m).unwrap()).unwrap().lines[0].line,
            2
        );
        assert!(matches!(
            scan(&file, 14, Some(15), &m).unwrap(),
            Outcome::TooLarge
        ));
        drop(file);
        let _ = std::fs::remove_file(&path);

        let (path, file) = temp_file("empty.txt", b"");
        assert!(found(scan(&file, 0, Some(0), &m).unwrap()).is_none());
        drop(file);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn size_limit_applies_to_large_files_with_late_hits() {
        let mut data = vec![b'.'; WINDOW * 2];
        data.extend_from_slice(b"\nneedle\n");
        let (path, file) = temp_file("late.txt", &data);
        let m = Matcher::new("needle").unwrap();
        assert!(matches!(
            scan(&file, WINDOW as u64, Some(1), &m).unwrap(),
            Outcome::TooLarge
        ));
        assert_eq!(
            found(scan(&file, u64::MAX, None, &m).unwrap())
                .unwrap()
                .lines[0]
                .line,
            2
        );
        drop(file);
        let _ = std::fs::remove_file(path);
    }
}
