use aho_corasick::{AhoCorasick, AhoCorasickKind, MatchKind};
use encoding_rs::{UTF_16BE, UTF_16LE, WINDOWS_1255};
use memchr::memmem::Finder;
use std::borrow::Cow;

const PREVIEW_CHARS: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf8Ignore,
    Utf16Le,
    Utf16Be,
    Cp1255,
}

impl Encoding {
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "utf-8",
            Encoding::Utf8Ignore => "utf-8-ignore",
            Encoding::Utf16Le => "utf-16-le",
            Encoding::Utf16Be => "utf-16-be",
            Encoding::Cp1255 => "cp1255",
        }
    }

    fn is_utf16(self) -> bool {
        matches!(self, Encoding::Utf16Le | Encoding::Utf16Be)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Bom {
    None,
    Utf8,
    Utf16Le,
    Utf16Be,
}

impl Bom {
    fn sniff(data: &[u8]) -> Bom {
        match data {
            [0xEF, 0xBB, 0xBF, ..] => Bom::Utf8,
            [0xFF, 0xFE, ..] => Bom::Utf16Le,
            [0xFE, 0xFF, ..] => Bom::Utf16Be,
            _ => Bom::None,
        }
    }

    fn len(self) -> usize {
        match self {
            Bom::None => 0,
            Bom::Utf8 => 3,
            Bom::Utf16Le | Bom::Utf16Be => 2,
        }
    }

    fn allows(self, enc: Encoding) -> bool {
        match self {
            Bom::None => true,
            Bom::Utf8 => enc == Encoding::Utf8,
            Bom::Utf16Le => enc == Encoding::Utf16Le,
            Bom::Utf16Be => enc == Encoding::Utf16Be,
        }
    }
}

struct Form {
    encoding: Encoding,
    finder: Finder<'static>,
}

enum Detector {
    Single(Box<Finder<'static>>),
    Multi(AhoCorasick),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LineHit {
    pub line: usize,
    pub preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatch {
    pub encoding: Encoding,
    pub lines: Vec<LineHit>,
}

pub struct Matcher {
    target: String,
    forms: Vec<Form>,
    detector: Detector,
    max_len: usize,
    searchable: bool,
}

impl Matcher {
    pub fn new(target: &str) -> Result<Matcher, String> {
        if target.is_empty() {
            return Err("the target string must not be empty".into());
        }

        let mut candidates: Vec<(Encoding, Vec<u8>)> = vec![
            (Encoding::Utf8, target.as_bytes().to_vec()),
            (
                Encoding::Utf16Le,
                target.encode_utf16().flat_map(u16::to_le_bytes).collect(),
            ),
            (
                Encoding::Utf16Be,
                target.encode_utf16().flat_map(u16::to_be_bytes).collect(),
            ),
        ];
        let (cp1255, _, unmappable) = WINDOWS_1255.encode(target);
        if !unmappable {
            candidates.push((Encoding::Cp1255, cp1255.into_owned()));
        }

        let mut patterns: Vec<Vec<u8>> = Vec::new();
        let mut forms = Vec::new();
        for (encoding, bytes) in candidates {
            if patterns.contains(&bytes) {
                continue;
            }
            forms.push(Form {
                encoding,
                finder: Finder::new(&bytes).into_owned(),
            });
            patterns.push(bytes);
        }

        let max_len = patterns.iter().map(Vec::len).max().unwrap_or(1);
        let detector = if patterns.len() == 1 {
            Detector::Single(Box::new(Finder::new(&patterns[0]).into_owned()))
        } else {
            let ac = AhoCorasick::builder()
                .match_kind(MatchKind::LeftmostFirst)
                .kind(Some(AhoCorasickKind::DFA))
                .build(&patterns)
                .map_err(|e| e.to_string())?;
            Detector::Multi(ac)
        };

        Ok(Matcher {
            target: target.to_owned(),
            forms,
            detector,
            max_len,
            searchable: !target.chars().any(is_line_boundary),
        })
    }

    pub fn max_len(&self) -> usize {
        self.max_len
    }

    pub fn first_candidate(&self, data: &[u8]) -> Option<usize> {
        match &self.detector {
            Detector::Single(finder) => finder.find(data),
            Detector::Multi(ac) => ac.find(data).map(|m| m.start()),
        }
    }

    pub fn scan(&self, data: &[u8]) -> Option<FileMatch> {
        if !self.searchable {
            return None;
        }
        let start = self.first_candidate(data)?;
        let bom = Bom::sniff(data);
        let body = &data[bom.len()..];
        let mut valid_utf8 = None;

        for form in &self.forms {
            if !bom.allows(form.encoding) || !self.present(form, data, start, bom) {
                continue;
            }
            let valid = *valid_utf8.get_or_insert_with(|| std::str::from_utf8(body).is_ok());
            let decodings: &[Encoding] = match form.encoding {
                Encoding::Cp1255 if valid => &[],
                Encoding::Utf8 if !valid && bom == Bom::None => {
                    &[Encoding::Cp1255, Encoding::Utf8Ignore]
                }
                Encoding::Utf8 if !valid => &[Encoding::Utf8Ignore],
                _ => std::slice::from_ref(&form.encoding),
            };
            for &encoding in decodings {
                let strict = form.encoding != encoding;
                let Some(text) = decode(encoding, body, strict) else {
                    continue;
                };
                let lines = matching_lines(&text, &self.target);
                if !lines.is_empty() {
                    return Some(FileMatch { encoding, lines });
                }
            }
        }
        None
    }

    fn present(&self, form: &Form, data: &[u8], start: usize, bom: Bom) -> bool {
        let base = bom.len();
        let mut pos = start.max(base);
        while pos < data.len() {
            let Some(found) = form.finder.find(&data[pos..]) else {
                return false;
            };
            let at = pos + found;
            if !form.encoding.is_utf16() || (at - base) % 2 == 0 {
                return true;
            }
            pos = at + 1;
        }
        false
    }
}

fn decode(encoding: Encoding, body: &[u8], strict: bool) -> Option<Cow<'_, str>> {
    Some(match encoding {
        Encoding::Utf8 => String::from_utf8_lossy(body),
        Encoding::Utf8Ignore => Cow::Owned(body.utf8_chunks().map(|c| c.valid()).collect()),
        Encoding::Utf16Le => UTF_16LE.decode_without_bom_handling(body).0,
        Encoding::Utf16Be => UTF_16BE.decode_without_bom_handling(body).0,
        Encoding::Cp1255 if strict => {
            WINDOWS_1255.decode_without_bom_handling_and_without_replacement(body)?
        }
        Encoding::Cp1255 => WINDOWS_1255.decode_without_bom_handling(body).0,
    })
}

fn is_line_boundary(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r'
            | '\x0b'
            | '\x0c'
            | '\x1c'
            | '\x1d'
            | '\x1e'
            | '\u{85}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn boundary_len(bytes: &[u8], i: usize) -> usize {
    match bytes[i] {
        b'\r' if bytes.get(i + 1) == Some(&b'\n') => 2,
        b'\n' | b'\r' | 0x0b | 0x0c | 0x1c | 0x1d | 0x1e => 1,
        0xC2 if bytes.get(i + 1) == Some(&0x85) => 2,
        0xE2 if bytes.get(i + 1) == Some(&0x80)
            && matches!(bytes.get(i + 2), Some(0xA8 | 0xA9)) =>
        {
            3
        }
        _ => 0,
    }
}

fn line_end(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && boundary_len(bytes, i) == 0 {
        i += 1;
    }
    i
}

fn matching_lines(text: &str, target: &str) -> Vec<LineHit> {
    let bytes = text.as_bytes();
    let mut hits = Vec::new();
    let mut line = 1;
    let mut line_start = 0;
    let mut cursor = 0;
    let mut reported = 0;

    for (at, _) in text.match_indices(target) {
        while cursor < at {
            match boundary_len(bytes, cursor) {
                0 => cursor += 1,
                n => {
                    cursor += n;
                    line += 1;
                    line_start = cursor;
                }
            }
        }
        if line == reported {
            continue;
        }
        reported = line;
        let end = line_end(bytes, at + target.len());
        hits.push(LineHit {
            line,
            preview: preview(&text[line_start..end]),
        });
    }
    hits
}

fn preview(line: &str) -> String {
    let trimmed = line.trim_matches(|c: char| c.is_whitespace() || ('\x1c'..='\x1f').contains(&c));
    match trimmed.char_indices().nth(PREVIEW_CHARS) {
        Some((cut, _)) => format!("{}…", &trimmed[..cut]),
        None => trimmed.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEBREW: &str = "סתם טקסט";

    fn lines(m: Option<FileMatch>) -> Vec<usize> {
        m.map(|m| m.lines.into_iter().map(|l| l.line).collect())
            .unwrap_or_default()
    }

    fn utf16le(s: &str, bom: bool) -> Vec<u8> {
        let mut out = if bom { vec![0xFF, 0xFE] } else { vec![] };
        out.extend(s.encode_utf16().flat_map(u16::to_le_bytes));
        out
    }

    fn utf16be(s: &str, bom: bool) -> Vec<u8> {
        let mut out = if bom { vec![0xFE, 0xFF] } else { vec![] };
        out.extend(s.encode_utf16().flat_map(u16::to_be_bytes));
        out
    }

    #[test]
    fn rejects_empty_target() {
        assert!(Matcher::new("").is_err());
    }

    #[test]
    fn ascii_target_collapses_utf8_and_cp1255() {
        let m = Matcher::new("needle").unwrap();
        assert_eq!(m.forms.len(), 3);
    }

    #[test]
    fn finds_every_encoding() {
        let m = Matcher::new(HEBREW).unwrap();
        let text = format!("first\nsecond {HEBREW} here\nthird");

        let r = m.scan(text.as_bytes()).unwrap();
        assert_eq!(r.encoding, Encoding::Utf8);
        assert_eq!(
            r.lines,
            vec![LineHit {
                line: 2,
                preview: format!("second {HEBREW} here")
            }]
        );

        let mut bom8 = vec![0xEF, 0xBB, 0xBF];
        bom8.extend(text.as_bytes());
        assert_eq!(m.scan(&bom8).unwrap().encoding, Encoding::Utf8);

        for (bytes, enc) in [
            (utf16le(&text, true), Encoding::Utf16Le),
            (utf16le(&text, false), Encoding::Utf16Le),
            (utf16be(&text, true), Encoding::Utf16Be),
            (utf16be(&text, false), Encoding::Utf16Be),
            (WINDOWS_1255.encode(&text).0.into_owned(), Encoding::Cp1255),
        ] {
            let r = m.scan(&bytes).unwrap();
            assert_eq!(r.encoding, enc);
            assert_eq!(lines(Some(r)), vec![2]);
        }
    }

    #[test]
    fn utf16_odd_offset_is_rejected() {
        let m = Matcher::new("ab").unwrap();
        let mut data = vec![b'x'];
        data.extend(utf16le("ab", false));
        data.push(0);
        assert!(m.scan(&data).is_none());
    }

    #[test]
    fn utf16_even_hit_after_odd_false_start() {
        let m = Matcher::new("\u{6161}").unwrap();
        let r = m.scan(&[0x78, 0x61, 0x61, 0x61]).unwrap();
        assert_eq!(r.encoding, Encoding::Utf16Le);
    }

    #[test]
    fn bom_restricts_encoding() {
        let m = Matcher::new("x").unwrap();
        let data = utf16be("x", true);
        assert_eq!(m.scan(&data).unwrap().encoding, Encoding::Utf16Be);
    }

    #[test]
    fn cp1255_found_in_legacy_file() {
        let m = Matcher::new(HEBREW).unwrap();
        let cp = WINDOWS_1255.encode(HEBREW).0.into_owned();
        assert!(std::str::from_utf8(&cp).is_err());
        assert!(m.scan(&cp).is_some());
    }

    #[test]
    fn ascii_target_in_legacy_hebrew_file_is_cp1255() {
        let m = Matcher::new("error").unwrap();
        let data = WINDOWS_1255.encode("שגיאה error").0.into_owned();
        let r = m.scan(&data).unwrap();
        assert_eq!(r.encoding, Encoding::Cp1255);
        assert_eq!(r.lines[0].preview, "שגיאה error");
    }

    #[test]
    fn stray_invalid_utf8_falls_back_to_ignore() {
        let m = Matcher::new(HEBREW).unwrap();
        let mut data = vec![0xC3, 0x28];
        data.extend_from_slice(format!(" broken\nx {HEBREW}\n").as_bytes());
        let r = m.scan(&data).unwrap();
        assert_eq!(r.encoding, Encoding::Utf8Ignore);
        assert_eq!(r.lines[0].line, 2);
        let ascii = Matcher::new("broken").unwrap();
        let mut data = vec![0xFF, 0xFB];
        data.extend_from_slice(b" broken\n");
        let r = ascii.scan(&data).unwrap();
        assert_eq!(r.encoding, Encoding::Utf8Ignore);
        assert_eq!(r.lines[0].preview, "broken");
    }

    #[test]
    fn python_splitlines_boundaries() {
        let m = Matcher::new("t").unwrap();
        let text = "a\r\nb\rc\x0bd\x0ce\x1cf\x1dg\x1eh\u{85}i\u{2028}j\u{2029}t\n";
        assert_eq!(lines(m.scan(text.as_bytes())), vec![11]);
    }

    #[test]
    fn one_row_per_line_and_every_line() {
        let m = Matcher::new("ab").unwrap();
        let text = "ab ab ab\nzz\nxxab\n\nab";
        assert_eq!(lines(m.scan(text.as_bytes())), vec![1, 3, 5]);
    }

    #[test]
    fn overlapping_occurrences() {
        let m = Matcher::new("aa").unwrap();
        assert_eq!(lines(m.scan(b"aaa\naaaa")), vec![1, 2]);
    }

    #[test]
    fn target_with_line_break_never_matches() {
        let m = Matcher::new("a\nb").unwrap();
        assert!(m.scan(b"a\nb").is_none());
    }

    #[test]
    fn preview_is_stripped_and_truncated() {
        let m = Matcher::new("k").unwrap();
        let long = format!("  \t{}k{}  ", "x".repeat(150), "y".repeat(150));
        let r = m.scan(long.as_bytes()).unwrap();
        let p = &r.lines[0].preview;
        assert!(p.starts_with('x') && p.ends_with('…'));
        assert_eq!(p.chars().count(), PREVIEW_CHARS + 1);
    }

    #[test]
    fn empty_and_miss() {
        let m = Matcher::new("zz").unwrap();
        assert!(m.scan(b"").is_none());
        assert!(m.scan(b"z z z").is_none());
    }
}
