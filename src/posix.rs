use crate::engine::Engine;
use rayon::Scope;
use std::collections::HashSet;
use std::fs::{self, File};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

const SKIP_DIRS: [&str; 7] = [
    "$recycle.bin",
    "system volume information",
    "proc",
    "dev",
    "sys",
    "run",
    "lost+found",
];
const FILE_BATCH: usize = 32;

pub fn discover_roots() -> Vec<PathBuf> {
    let mut roots = vec![PathBuf::from("/")];
    for base in ["/mnt", "/media", "/Volumes"] {
        let Ok(entries) = fs::read_dir(base) else {
            continue;
        };
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                roots.push(entry.path());
            }
        }
    }
    roots
}

pub fn cpu_time() -> Option<(std::time::Duration, std::time::Duration)> {
    None
}

pub fn physical_cores() -> Option<usize> {
    None
}

pub fn enter_background() -> bool {
    false
}

pub fn volume_key(_path: &Path) -> Option<String> {
    None
}

pub fn privilege_note() -> Option<String> {
    None
}

pub struct Root {
    path: PathBuf,
}

impl Root {
    pub fn describe(&self) -> String {
        format!("{}  [walk]", self.path.display())
    }
}

pub fn plan(roots: Vec<PathBuf>) -> Vec<Root> {
    roots.into_iter().map(|path| Root { path }).collect()
}

struct Walk<'s> {
    engine: &'s Engine,
    visited: Option<Mutex<HashSet<(u64, u64)>>>,
}

impl Walk<'_> {
    fn first_visit(&self, path: &Path) -> bool {
        let Some(visited) = &self.visited else {
            return true;
        };
        match fs::metadata(path) {
            Ok(m) => visited
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert((m.dev(), m.ino())),
            Err(_) => false,
        }
    }
}

pub fn run(engine: &Engine, roots: Vec<Root>) {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(engine.cfg.workers)
        .build()
        .expect("thread pool");
    let walk = Arc::new(Walk {
        engine,
        visited: engine
            .cfg
            .follow_symlinks
            .then(|| Mutex::new(HashSet::new())),
    });
    pool.scope(|scope| {
        for root in roots {
            if let Err(e) = fs::read_dir(&root.path) {
                engine
                    .console
                    .warn(&format!("Could not walk {}: {e}", root.path.display()));
                continue;
            }
            if walk.first_visit(&root.path) {
                let walk = walk.clone();
                scope.spawn(move |scope| walk_dir(walk, scope, root.path));
            }
        }
    });
}

fn skip_dir(name: &[u8]) -> bool {
    SKIP_DIRS
        .iter()
        .any(|s| s.as_bytes().eq_ignore_ascii_case(name))
}

fn walk_dir<'s>(walk: Arc<Walk<'s>>, scope: &Scope<'s>, dir: PathBuf) {
    let engine = walk.engine;
    let Ok(entries) = fs::read_dir(&dir) else {
        engine.stats.failed.inc();
        return;
    };

    let mut batch = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.as_bytes();
        if !engine.cfg.include_hidden && name.starts_with(b".") {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            continue;
        };
        let path = entry.path();
        let (is_dir, is_file, linked) = if kind.is_symlink() {
            match fs::metadata(&path) {
                Ok(m) => (m.is_dir(), m.is_file(), true),
                Err(_) => continue,
            }
        } else {
            (kind.is_dir(), kind.is_file(), false)
        };

        if is_dir {
            if (linked && !engine.cfg.follow_symlinks) || skip_dir(name) || !walk.first_visit(&path)
            {
                continue;
            }
            let walk = walk.clone();
            scope.spawn(move |scope| walk_dir(walk, scope, path));
        } else if is_file && engine.wants(name) {
            engine.stats.listed.inc();
            batch.push(path);
            if batch.len() == FILE_BATCH {
                spawn_files(engine, scope, std::mem::take(&mut batch));
            }
        }
    }
    if !batch.is_empty() {
        spawn_files(engine, scope, batch);
    }
}

fn spawn_files<'s>(engine: &'s Engine, scope: &Scope<'s>, batch: Vec<PathBuf>) {
    scope.spawn(move |_| {
        for path in batch {
            engine.process(File::open(&path), None, || path.display().to_string());
        }
    });
}
