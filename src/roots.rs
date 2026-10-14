use crate::report::Console;
use std::path::{MAIN_SEPARATOR, Path, PathBuf};

fn expand_bare_drive(path: PathBuf) -> PathBuf {
    if cfg!(windows) {
        let s = path.to_string_lossy();
        let b = s.as_bytes();
        if b.len() == 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return PathBuf::from(format!("{s}\\"));
        }
    }
    path
}

fn key(path: &Path) -> String {
    let mut s = path.to_string_lossy().into_owned();
    if cfg!(windows) {
        s = s.replace('/', "\\").to_lowercase();
    }
    if !s.ends_with(MAIN_SEPARATOR) {
        s.push(MAIN_SEPARATOR);
    }
    s
}

pub fn normalize(
    requested: Vec<PathBuf>,
    console: &Console,
    volume: impl Fn(&Path) -> Option<String>,
) -> Vec<PathBuf> {
    let mut roots: Vec<(String, PathBuf)> = Vec::new();
    for raw in requested {
        let path = match std::path::absolute(expand_bare_drive(raw.clone())) {
            Ok(p) if p.is_dir() => p,
            _ => {
                console.warn(&format!(
                    "Could not walk {}: not an accessible directory",
                    raw.display()
                ));
                continue;
            }
        };
        roots.push((key(&path), path));
    }
    roots.sort_by(|a, b| a.0.len().cmp(&b.0.len()).then_with(|| a.0.cmp(&b.0)));

    let mut kept: Vec<(String, PathBuf, Option<String>)> = Vec::new();
    for (k, path) in roots {
        let vol = volume(&path);
        let covered = kept
            .iter()
            .any(|(parent, _, parent_vol)| k.starts_with(parent.as_str()) && *parent_vol == vol);
        if !covered {
            kept.push((k, path, vol));
        }
    }
    kept.into_iter().map(|(_, p, _)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_and_duplicate_roots_collapse() {
        let base = std::env::temp_dir();
        let child = base.join("stringfinder-roots-test");
        std::fs::create_dir_all(&child).unwrap();
        let console = Console::new();
        let requested = vec![child.clone(), base.clone(), base.clone()];
        let roots = normalize(requested.clone(), &console, |_| None);
        assert_eq!(roots.len(), 1);
        assert_eq!(key(&roots[0]), key(&std::path::absolute(&base).unwrap()));

        let roots = normalize(requested, &console, |p| Some(key(p)));
        assert_eq!(roots.len(), 2);
    }
}
