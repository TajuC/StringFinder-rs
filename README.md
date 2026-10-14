# StringFinder

Finds a string inside every text file on every drive and writes the hits to a CSV report. It is the native successor of the Python [StringFinder](https://github.com/TajuC/StringFinder): same flags, same report format, a fraction of the time.

## How it works

* **Discovery** walks every drive in parallel on a work-stealing pool. On Windows it talks to NTFS directly: directories are listed with `NtQueryDirectoryFile` into 64 KB buffers (names, attributes, sizes and reparse tags in one call, so no per-file `stat`), and every child is opened with `NtCreateFile` relative to its parent's handle, which skips Win32 path parsing and the per-component lookup from the drive root. Volumes mounted into folders are discovered with `FindFirstVolumeW` and scanned as roots of their own.
* **Reading** uses one reusable 4 KB-aligned buffer per thread. The size reported by the directory listing only sizes the first request, so a typical file costs exactly one open, one read and one close; reads always continue to the real end of file, which keeps symlinks and files that are still growing correct. Large files stream through 4 MB windows that carry the last `pattern - 1` bytes forward, so a hit straddling two windows is never missed.
* **Matching** never decodes a file just to search it. The target is encoded once as UTF-8, UTF-16 LE, UTF-16 BE and Windows-1255, and all forms are searched in a single pass over the raw bytes with SIMD (`memchr` two-way search for one form, the Teddy multi-pattern searcher from `aho-corasick` for several, both with runtime AVX2 dispatch). Only files that contain a candidate are decoded, and their line numbers are computed with the exact boundary rules of Python's `str.splitlines`.
* **Windows specifics**: `SeBackupPrivilege` is enabled when the process is elevated, so ACL-protected files are readable; cloud placeholder files (OneDrive and friends) are skipped instead of being downloaded; drives with a seek penalty are detected with `IOCTL_STORAGE_QUERY_PROPERTY` and read by a single thread in directory order instead of being thrashed by parallel reads.

## Usage

```
stringfinder --target "your text here"
stringfinder --target "needle" --roots C:\ D:\ --output results.csv
stringfinder --target "needle" --ext txt,log,md --max-file-bytes 104857600
```

| Option | Meaning |
| --- | --- |
| `--target <TEXT>` | Substring to search for |
| `--roots <PATH>...` | Starting roots. All fixed, removable and network drives when omitted (`/`, `/mnt/*`, `/media/*`, `/Volumes/*` on Linux and macOS) |
| `--ext <EXT>[,<EXT>...]` | File extensions to scan, default `txt` |
| `--max-file-bytes <N>` | Skip larger files, default 52428800 (50 MB) |
| `--workers <N>` | Reader threads, default is the number of physical cores |
| `--background` | Run at background CPU and I/O priority so the machine stays responsive |
| `--output <FILE>` | CSV report path, default `matches.csv` |
| `--follow-symlinks` | Follow symlinks and junctions (loops are detected) |
| `--include-hidden` | Include dot files and dot directories on Linux and macOS |
| `--include-offline` | Read cloud placeholder files, which forces a download |

Run it from an elevated terminal on Windows for full coverage.

## Performance

Measured on Windows 10, i9-11900 (8 cores, 16 threads), Samsung 980 and Gigabyte NVMe drives plus a SATA SSD, with the default 8 workers. "Cold" means the standby list and system cache working set were purged right before the run.

| Workload | Python version | StringFinder, warm | StringFinder, cold |
| --- | --- | --- | --- |
| 200,020 `.txt` files, 1.2 GB, one folder tree | 25.8 s warm, 28.5 s cold | 1.1 s | 6.2 to 6.5 s |
| Whole machine: 7 drives, 211,357 `.txt` files, 1.8 GB | not measured | 2.9 s | 12.1 to 14.0 s |

StringFinder reports every line the Python version finds on that corpus, with identical line numbers and previews, plus five lines the Python version misses (see below).

Where the CPU goes: about 95% of the CPU time is spent inside the Windows kernel opening, reading and closing files and listing directories (NTFS plus the file system filter stack, Defender included). The search itself takes well under a second of CPU for the whole corpus. More threads than physical cores finishes slightly sooner but burns noticeably more CPU on kernel lock contention, which is why the default is one thread per physical core. Use `--workers` to trade CPU for speed, or `--background` to let other programs come first.

### Output

```
[HIT] C:\Users\me\notes\todo.txt  (encoding=utf-8)  lines=2,4
```

`matches.csv` has one row per matching line with the columns `path, encoding, line_number, line_preview`, exactly like the Python version. The summary at the end reports files scanned, bytes read, throughput, CPU time and anything that was skipped.

## Differences from the Python version

* Encodings are reported per form that matched: `utf-16-le` and `utf-16-be` instead of a generic `utf-16`.
* Files the Python version decoded wrongly are now found: Windows-1255 files with an even byte count and files with stray invalid UTF-8 were silently decoded as UTF-16 garbage there. Such files are reported as `cp1255` or `utf-8-ignore`.
* Symlinks, junctions and mount points inside a tree are not followed unless `--follow-symlinks` is given, so nothing is scanned twice and junction loops cannot happen. Their targets are still scanned at their real location, and folder-mounted volumes are scanned as their own roots. Cloud-synced folders such as OneDrive are walked normally.
* Folders named `dev`, `proc`, `sys`, `run` and `lost+found` are only skipped on Linux and macOS; on Windows they are ordinary folders.
* Overlapping roots (`C:\` and `C:\Users`) are scanned once.

## Building

```
cargo build --release
```

The binary lands in `target/release/`. Requires Rust 1.85 or newer.

## License

MIT
