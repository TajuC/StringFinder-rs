use lexopt::prelude::*;
use std::path::PathBuf;

pub const DEFAULT_TARGET: &str = "סתם טקסט";

#[derive(Debug)]
pub struct Config {
    pub target: String,
    pub max_file_bytes: u64,
    pub workers: usize,
    pub follow_symlinks: bool,
    pub include_hidden: bool,
    pub include_offline: bool,
    pub roots: Vec<PathBuf>,
    pub output: PathBuf,
    pub extensions: Vec<String>,
    pub background: bool,
}

const HELP: &str = "\
Search every accessible drive for a string inside text files.

Usage: stringfinder [OPTIONS]

Options:
  --target <TEXT>          Substring to search for [default: built-in sample]
  --roots <PATH>...        Starting roots; all drives when omitted
  --ext <EXT>[,<EXT>...]   File extensions to scan [default: txt]
  --max-file-bytes <N>     Skip files larger than N bytes [default: 52428800]
  --workers <N>            Parallel reader threads [default: physical cores]
  --background             Run at background CPU and I/O priority
  --output <FILE>          CSV report path [default: matches.csv]
  --follow-symlinks        Follow symlinks and junctions while walking
  --include-hidden         Include dot files and dot directories (Linux, macOS)
  --include-offline        Read cloud placeholder files (forces a download)
  -h, --help               Print help
  -V, --version            Print version
";

fn default_workers() -> usize {
    crate::platform::physical_cores()
        .or_else(|| std::thread::available_parallelism().ok().map(|n| n.get()))
        .unwrap_or(4)
        .clamp(1, 64)
}

impl Config {
    fn defaults() -> Config {
        Config {
            target: DEFAULT_TARGET.to_owned(),
            max_file_bytes: 50 * 1024 * 1024,
            workers: default_workers(),
            follow_symlinks: false,
            include_hidden: false,
            include_offline: false,
            roots: Vec::new(),
            output: PathBuf::from("matches.csv"),
            extensions: Vec::new(),
            background: false,
        }
    }

    #[cfg(test)]
    pub fn for_tests() -> Config {
        let mut cfg = Config::defaults();
        cfg.extensions.push("txt".into());
        cfg
    }
}

pub fn parse() -> Result<Config, lexopt::Error> {
    let mut cfg = Config::defaults();

    let mut parser = lexopt::Parser::from_env();
    while let Some(arg) = parser.next()? {
        match arg {
            Long("target") => cfg.target = parser.value()?.string()?,
            Long("max-file-bytes") => cfg.max_file_bytes = parser.value()?.parse()?,
            Long("workers") => {
                let n: usize = parser.value()?.parse()?;
                if n == 0 {
                    return Err("--workers must be at least 1".into());
                }
                cfg.workers = n;
            }
            Long("follow-symlinks") => cfg.follow_symlinks = true,
            Long("include-hidden") => cfg.include_hidden = true,
            Long("include-offline") => cfg.include_offline = true,
            Long("roots") => cfg.roots.extend(parser.values()?.map(PathBuf::from)),
            Long("output") => cfg.output = parser.value()?.into(),
            Long("ext") => {
                let list = parser.value()?.string()?;
                cfg.extensions.extend(
                    list.split(',')
                        .map(|e| e.trim().trim_start_matches('.').to_lowercase())
                        .filter(|e| !e.is_empty()),
                );
            }
            Long("background") => cfg.background = true,
            Short('h') | Long("help") => {
                print!("{HELP}");
                std::process::exit(0);
            }
            Short('V') | Long("version") => {
                println!("stringfinder {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            _ => return Err(arg.unexpected()),
        }
    }

    if cfg.extensions.is_empty() {
        cfg.extensions.push("txt".into());
    }
    cfg.extensions.sort_unstable();
    cfg.extensions.dedup();
    Ok(cfg)
}
