mod cli;
mod engine;
mod matcher;
mod report;
mod roots;
mod scan;

#[cfg(not(windows))]
mod posix;
#[cfg(windows)]
mod windows;

#[cfg(not(windows))]
use posix as platform;
#[cfg(windows)]
use windows as platform;

use engine::Engine;
use matcher::Matcher;
use report::{Console, group, human_bytes};
use std::process::ExitCode;
use std::sync::mpsc;
use std::time::Instant;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = cli::parse()?;
    let matcher = Matcher::new(&cfg.target)?;
    let console: &'static Console = Box::leak(Box::new(Console::new()));

    let requested = if cfg.roots.is_empty() {
        platform::discover_roots()
    } else {
        cfg.roots.clone()
    };
    let roots = platform::plan(roots::normalize(requested, console, platform::volume_key));
    if roots.is_empty() {
        return Err("no readable roots to scan".into());
    }

    println!("Scanning roots:");
    for root in &roots {
        println!("  - {}", root.describe());
    }
    println!("\nTarget length: {} chars", cfg.target.chars().count());
    println!(
        "Workers: {} | Max file size: {} bytes | Extensions: {}",
        cfg.workers,
        cfg.max_file_bytes,
        cfg.extensions.join(",")
    );
    if let Some(note) = platform::privilege_note() {
        println!("{note}");
    }
    if cfg.background && !platform::enter_background() {
        console.warn("background priority is not available on this platform");
    }
    println!();

    let output = std::path::absolute(&cfg.output)?;
    let (hit_tx, hit_rx) = mpsc::channel();
    let writer = report::spawn_writer(&output, hit_rx, console)
        .map_err(|e| format!("cannot create report {}: {e}", output.display()))?;

    let engine = Engine::new(cfg, matcher, console, hit_tx);
    let started = Instant::now();
    std::thread::scope(|s| {
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let engine = &engine;
        s.spawn(move || report::progress(&engine.stats, console, started, stop_rx));
        platform::run(engine, roots);
        drop(stop_tx);
    });
    let elapsed = started.elapsed().as_secs_f64();
    console.finish();

    let stats = &engine.stats;
    let (listed, scanned, bytes) = (stats.listed.get(), stats.scanned.get(), stats.bytes.get());
    let (too_large, offline, failed) = (
        stats.too_large.get(),
        stats.offline.get(),
        stats.failed.get(),
    );
    drop(engine);

    let totals = writer.join().map_err(|_| "report writer panicked")??;
    println!(
        "\nDone. Matches: {} lines in {} files.",
        totals.lines, totals.files
    );
    println!("Report written to: {}", output.display());
    println!(
        "Scanned {} of {} candidate files, {} in {:.2}s ({:.0} files/s, {}/s)",
        group(scanned),
        group(listed),
        human_bytes(bytes),
        elapsed,
        scanned as f64 / elapsed.max(1e-6),
        human_bytes((bytes as f64 / elapsed.max(1e-6)) as u64),
    );
    if let Some((kernel, user)) = platform::cpu_time() {
        let total = (kernel + user).as_secs_f64();
        println!(
            "CPU time: {total:.2}s (kernel {:.2}s, user {:.2}s, average {:.1} cores busy)",
            kernel.as_secs_f64(),
            user.as_secs_f64(),
            total / elapsed.max(1e-6)
        );
    }
    if too_large + offline + failed > 0 {
        println!(
            "Skipped: {} over the size limit, {} cloud-only, {} unreadable",
            group(too_large),
            group(offline),
            group(failed)
        );
    }
    Ok(())
}
