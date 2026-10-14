pub mod ffi;
mod volume;

use crate::engine::Engine;
use ffi::{Handle, Identity, from_wide, identity, list_dir, open_dir, open_relative};
use rayon::Scope;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_OFFLINE, FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS,
    FILE_ATTRIBUTE_RECALL_ON_OPEN,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, PROCESS_MODE_BACKGROUND_BEGIN, SetPriorityClass,
};

const OFFLINE: u32 =
    FILE_ATTRIBUTE_OFFLINE | FILE_ATTRIBUTE_RECALL_ON_OPEN | FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS;
const FILE_BATCH: usize = 256;
const NAME_SURROGATE: u32 = 0x2000_0000;
const SKIP_DIRS: [&str; 2] = ["$recycle.bin", "system volume information"];

static BACKUP_PRIVILEGE: OnceLock<bool> = OnceLock::new();

fn skip_dir(name: &[u16]) -> bool {
    SKIP_DIRS.iter().any(|skip| {
        skip.len() == name.len()
            && skip
                .bytes()
                .zip(name)
                .all(|(s, &n)| n < 0x80 && (n as u8).to_ascii_lowercase() == s)
    })
}

pub fn discover_roots() -> Vec<PathBuf> {
    let mut roots = volume::logical_drives();
    roots.extend(volume::mounted_folders());
    roots
}

pub fn volume_key(path: &std::path::Path) -> Option<String> {
    volume::mount_point(&path.to_string_lossy())
}

pub fn cpu_time() -> Option<(std::time::Duration, std::time::Duration)> {
    ffi::cpu_time()
}

pub fn physical_cores() -> Option<usize> {
    ffi::physical_cores()
}

pub fn enter_background() -> bool {
    unsafe { SetPriorityClass(GetCurrentProcess(), PROCESS_MODE_BACKGROUND_BEGIN) != 0 }
}

pub fn privilege_note() -> Option<String> {
    BACKUP_PRIVILEGE.get().map(|on| {
        format!(
            "Backup privilege: {}",
            if *on {
                "enabled"
            } else {
                "unavailable (not elevated)"
            }
        )
    })
}

pub struct Root {
    path: String,
    seek_penalty: bool,
    filesystem: String,
}

impl Root {
    pub fn describe(&self) -> String {
        let fs = if self.filesystem.is_empty() {
            "unknown fs"
        } else {
            &self.filesystem
        };
        let media = if self.seek_penalty {
            ", rotational"
        } else {
            ""
        };
        format!("{}  [{fs}{media}]", self.path)
    }
}

pub fn plan(roots: Vec<PathBuf>) -> Vec<Root> {
    BACKUP_PRIVILEGE.get_or_init(volume::enable_backup_privilege);
    roots
        .into_iter()
        .map(|path| {
            let path = path.to_string_lossy().into_owned();
            match volume::volume_of(&path) {
                Some(vol) => Root {
                    seek_penalty: vol
                        .device
                        .as_deref()
                        .and_then(volume::seek_penalty)
                        .unwrap_or(false),
                    filesystem: vol.filesystem,
                    path,
                },
                None => Root {
                    path,
                    seek_penalty: false,
                    filesystem: String::new(),
                },
            }
        })
        .collect()
}

pub fn run(engine: &Engine, roots: Vec<Root>) {
    let (rotational, solid): (Vec<Root>, Vec<Root>) =
        roots.into_iter().partition(|r| r.seek_penalty);
    std::thread::scope(|s| {
        for root in rotational {
            s.spawn(move || {
                let pool = rayon::ThreadPoolBuilder::new()
                    .num_threads(1)
                    .build()
                    .expect("thread pool");
                pool.scope(|scope| walk_root(engine, scope, &root.path));
            });
        }
        if !solid.is_empty() {
            let pool = rayon::ThreadPoolBuilder::new()
                .num_threads(engine.cfg.workers)
                .build()
                .expect("thread pool");
            pool.scope(|scope| {
                for root in &solid {
                    scope.spawn(move |scope| walk_root(engine, scope, &root.path));
                }
            });
        }
    });
}

struct Dir {
    handle: Handle,
    path: String,
}

struct Walk<'s> {
    engine: &'s Engine,
    visited: Option<Mutex<HashSet<Identity>>>,
}

impl Walk<'_> {
    fn first_visit(&self, handle: &Handle) -> bool {
        let Some(visited) = &self.visited else {
            return true;
        };
        match identity(handle) {
            Ok(id) => visited.lock().unwrap_or_else(|e| e.into_inner()).insert(id),
            Err(_) => true,
        }
    }
}

fn walk_root<'s>(engine: &'s Engine, scope: &Scope<'s>, path: &str) {
    let handle = match open_dir(path) {
        Ok(h) => h,
        Err(e) => {
            engine.console.warn(&format!("Could not walk {path}: {e}"));
            return;
        }
    };
    let walk = Walk {
        engine,
        visited: engine
            .cfg
            .follow_symlinks
            .then(|| Mutex::new(HashSet::new())),
    };
    walk.first_visit(&handle);
    let path = path.trim_end_matches('\\').to_owned();
    walk_dir(Arc::new(walk), scope, Arc::new(Dir { handle, path }));
}

fn walk_dir<'s>(walk: Arc<Walk<'s>>, scope: &Scope<'s>, dir: Arc<Dir>) {
    let engine = walk.engine;
    let cfg = &engine.cfg;
    let mut batch: Vec<(Vec<u16>, u64)> = Vec::new();
    let listed = list_dir(&dir.handle, |entry| {
        if entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            let link = entry.reparse_tag & NAME_SURROGATE != 0;
            if (link && !cfg.follow_symlinks) || skip_dir(entry.name) {
                return;
            }
            let (walk, parent, name) = (walk.clone(), dir.clone(), entry.name.to_vec());
            scope.spawn(move |scope| {
                let handle = match open_relative(&parent.handle, &name, true) {
                    Ok(handle) => handle,
                    Err(_) => {
                        walk.engine.stats.failed.inc();
                        return;
                    }
                };
                if !walk.first_visit(&handle) {
                    return;
                }
                let path = format!("{}\\{}", parent.path, from_wide(&name));
                drop(parent);
                walk_dir(walk, scope, Arc::new(Dir { handle, path }));
            });
        } else if engine.wants_wide(entry.name) {
            engine.stats.listed.inc();
            if entry.attributes & OFFLINE != 0 && !cfg.include_offline {
                engine.stats.offline.inc();
                return;
            }
            batch.push((entry.name.to_vec(), entry.size));
            if batch.len() == FILE_BATCH {
                spawn_files(engine, scope, &dir, std::mem::take(&mut batch));
            }
        }
    });
    if listed.is_err() {
        engine.stats.failed.inc();
    }
    if !batch.is_empty() {
        spawn_files(engine, scope, &dir, batch);
    }
}

fn spawn_files<'s>(
    engine: &'s Engine,
    scope: &Scope<'s>,
    dir: &Arc<Dir>,
    batch: Vec<(Vec<u16>, u64)>,
) {
    let dir = dir.clone();
    scope.spawn(move |_| {
        for (name, size) in batch {
            let opened = open_relative(&dir.handle, &name, false).map(Handle::into_file);
            engine.process(opened, Some(size), || {
                format!("{}\\{}", dir.path, from_wide(&name))
            });
        }
    });
}
