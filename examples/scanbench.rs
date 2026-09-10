//! Catalog-scan benchmark for the header-only ingest path (`Catalog::scan`).
//!
//! Three modes, matching the language server's catalog startup and the
//! catalog-scan regression report (`netconf-language-server/docs/perf/`):
//!
//! - `seq`       — per-file sequential read + `Catalog::scan`. Each scan is
//!   timed individually, so this mode also feeds the slowest-file list (the
//!   worst-file metric).
//! - `par`       — `CatalogIndex::scan_many_files` (rayon with the `parallel`
//!   feature; a plain sequential loop otherwise).
//! - `par-canon` — `CatalogIndex::scan_many_files_with` with language-server
//!   style canonical `file://` urls (canonicalization runs inside the
//!   workers).
//!
//! Usage:
//!   scanbench <dir> [--skip N] [--limit M] [--file-list f]
//!             [--mode seq|par|par-canon|all] [--top N]
//!
//! `--file-list` supplies explicit paths (one per line) instead of walking
//! `<dir>`, so `<dir>` may be omitted then; `--skip`/`--limit` apply to the
//! sorted path list either way. `--top 0` disables the slowest-file list.
//! `--mode all` (the default) runs `seq`, then `par`, then `par-canon` in one
//! process, so the later modes inherit the process high-water mark — run a
//! single `--mode` per process for a clean per-mode VmHWM.
//!
//! Reported per mode: files, source MB, ms/file, MB/s, user/sys CPU seconds
//! (from `/proc/self/stat`, whole process incl. worker threads), VmHWM/VmRSS,
//! and (seq) the top-N slowest per-file `Catalog::scan` times.

use std::path::{Path, PathBuf};
use std::time::Instant;

use yrepo::{Catalog, CatalogIndex};

/// Resident set size in kB from /proc/self/status (Linux).
fn proc_field(field: &str) -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix(field) {
            let v: String = rest
                .trim()
                .trim_end_matches(" kB")
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            return v.parse().ok();
        }
    }
    None
}

fn rss_kb() -> u64 {
    proc_field("VmRSS:").unwrap_or(0)
}

fn hwm_kb() -> u64 {
    proc_field("VmHWM:").unwrap_or(0)
}

/// (user, sys) CPU seconds from /proc/self/stat (fields 14/15, clock ticks).
fn cpu_seconds() -> Option<(f64, f64)> {
    let stat = std::fs::read_to_string("/proc/self/stat").ok()?;
    // `comm` (field 2) may contain spaces/parens; fields restart after the
    // last ')'. Tokens after it: index 0 == field 3 (state), so field 14
    // (utime) is index 11 and field 15 (stime) is index 12.
    let after = stat.split(')').next_back()?;
    let ticks: Vec<&str> = after.split_whitespace().collect();
    let utime: u64 = ticks.get(11)?.parse().ok()?;
    let stime: u64 = ticks.get(12)?.parse().ok()?;
    Some((utime as f64 / 100.0, stime as f64 / 100.0))
}

fn walk(root: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "yang") {
                out.push(p);
            }
        }
    }
}

fn read_file_list(path: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(path) else {
        eprintln!("scanbench: cannot read file list {}", path.display());
        std::process::exit(2);
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// One mode's measurement.
struct Stats {
    files: usize,
    bytes: u64,
    wall_s: f64,
    user_s: f64,
    sys_s: f64,
    hwm_kb: u64,
    rss_kb: u64,
    scanned: usize,
}

fn report(mode: &str, s: &Stats) {
    let mb = s.bytes as f64 / 1_048_576.0;
    let per_file_ms = if s.files == 0 {
        0.0
    } else {
        s.wall_s * 1000.0 / s.files as f64
    };
    let mb_s = if s.wall_s > 0.0 { mb / s.wall_s } else { 0.0 };
    println!("== {mode} ==");
    println!(
        "files: {} (scanned {})  source: {:.1} MB ({} bytes)",
        s.files, s.scanned, mb, s.bytes
    );
    println!(
        "wall: {:.3} s  {:.3} ms/file  {:.2} MB/s",
        s.wall_s, per_file_ms, mb_s
    );
    println!("cpu: user {:.3} s  sys {:.3} s", s.user_s, s.sys_s);
    println!(
        "mem: VmHWM {:.1} MB  VmRSS {:.1} MB",
        s.hwm_kb as f64 / 1024.0,
        s.rss_kb as f64 / 1024.0
    );
    println!();
}

/// Insert `entry` into a descending-by-time, at-most-`n`-long slowest list.
fn insert_slowest(top: &mut Vec<(u128, u64, PathBuf)>, entry: (u128, u64, PathBuf), n: usize) {
    if n == 0 {
        return;
    }
    let pos = top
        .iter()
        .position(|existing| existing.0 < entry.0)
        .unwrap_or(top.len());
    top.insert(pos, entry);
    top.truncate(n);
}

/// Sequential per-file read + `Catalog::scan`, timing each scan.
fn run_seq(files: &[(PathBuf, u64)], top_n: usize) -> (Stats, Vec<(u128, u64, PathBuf)>) {
    let (u0, s0) = cpu_seconds().unwrap_or((0.0, 0.0));
    let bytes: u64 = files.iter().map(|(_, b)| *b).sum();
    let mut index = CatalogIndex::default();
    let mut top: Vec<(u128, u64, PathBuf)> = Vec::new();
    let mut scanned = 0usize;
    let wall0 = Instant::now();
    for (path, size) in files {
        let Ok(source) = std::fs::read_to_string(path) else {
            continue;
        };
        let t0 = Instant::now();
        let catalog = Catalog::scan(path.to_string_lossy().to_string(), source);
        let elapsed = t0.elapsed().as_nanos();
        index.push(catalog);
        scanned += 1;
        insert_slowest(&mut top, (elapsed, *size, path.clone()), top_n);
    }
    let wall_s = wall0.elapsed().as_secs_f64();
    drop(index);
    let (u1, s1) = cpu_seconds().unwrap_or((0.0, 0.0));
    let stats = Stats {
        files: files.len(),
        bytes,
        wall_s,
        user_s: (u1 - u0).max(0.0),
        sys_s: (s1 - s0).max(0.0),
        hwm_kb: hwm_kb(),
        rss_kb: rss_kb(),
        scanned,
    };
    (stats, top)
}

/// `CatalogIndex::scan_many_files` (rayon when the `parallel` feature is on).
fn run_par(files: &[(PathBuf, u64)]) -> Stats {
    let (u0, s0) = cpu_seconds().unwrap_or((0.0, 0.0));
    let bytes: u64 = files.iter().map(|(_, b)| *b).sum();
    let mut index = CatalogIndex::default();
    let wall0 = Instant::now();
    let n = index.scan_many_files(files.iter().map(|(p, _)| p));
    let wall_s = wall0.elapsed().as_secs_f64();
    drop(index);
    let (u1, s1) = cpu_seconds().unwrap_or((0.0, 0.0));
    Stats {
        files: files.len(),
        bytes,
        wall_s,
        user_s: (u1 - u0).max(0.0),
        sys_s: (s1 - s0).max(0.0),
        hwm_kb: hwm_kb(),
        rss_kb: rss_kb(),
        scanned: n,
    }
}

/// `CatalogIndex::scan_many_files_with` + canonical `file://` urls.
fn run_par_canon(files: &[(PathBuf, u64)]) -> Stats {
    let (u0, s0) = cpu_seconds().unwrap_or((0.0, 0.0));
    let bytes: u64 = files.iter().map(|(_, b)| *b).sum();
    let mut index = CatalogIndex::default();
    let wall0 = Instant::now();
    let n = index.scan_many_files_with(files.iter().map(|(p, _)| p), |p| {
        let canon = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
        Some(format!("file://{}", canon.display()))
    });
    let wall_s = wall0.elapsed().as_secs_f64();
    drop(index);
    let (u1, s1) = cpu_seconds().unwrap_or((0.0, 0.0));
    Stats {
        files: files.len(),
        bytes,
        wall_s,
        user_s: (u1 - u0).max(0.0),
        sys_s: (s1 - s0).max(0.0),
        hwm_kb: hwm_kb(),
        rss_kb: rss_kb(),
        scanned: n,
    }
}

fn main() {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut dir: Option<String> = None;
    let mut file_list: Option<PathBuf> = None;
    let mut skip = 0usize;
    let mut limit = usize::MAX;
    let mut top_n = 10usize;
    let mut mode = String::from("all");

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--skip" => {
                i += 1;
                skip = raw.get(i).and_then(|s| s.parse().ok()).unwrap_or(0);
            }
            "--limit" => {
                i += 1;
                limit = raw
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(usize::MAX);
            }
            "--top" => {
                i += 1;
                top_n = raw.get(i).and_then(|s| s.parse().ok()).unwrap_or(10);
            }
            "--file-list" => {
                i += 1;
                file_list = raw.get(i).map(PathBuf::from);
            }
            "--mode" => {
                i += 1;
                mode = raw.get(i).cloned().unwrap_or_else(|| "all".to_string());
            }
            a if a.starts_with('-') => {
                eprintln!("scanbench: unknown flag {a}");
                std::process::exit(2);
            }
            d => dir = Some(d.to_string()),
        }
        i += 1;
    }

    let modes: Vec<&str> = match mode.as_str() {
        "all" => vec!["seq", "par", "par-canon"],
        "seq" | "par" | "par-canon" => vec![mode.as_str()],
        other => {
            eprintln!("scanbench: unknown mode {other}");
            std::process::exit(2);
        }
    };

    // ---- collect the path list ----
    let mut paths: Vec<PathBuf> = if let Some(list) = &file_list {
        read_file_list(list)
    } else if let Some(dir) = &dir {
        let mut files = Vec::new();
        walk(Path::new(dir), &mut files);
        files
    } else {
        eprintln!(
            "usage: scanbench <dir> [--skip N] [--limit M] [--file-list f] \
             [--mode seq|par|par-canon|all] [--top N]"
        );
        std::process::exit(2);
    };
    paths.sort();
    paths.dedup();
    let files: Vec<(PathBuf, u64)> = paths
        .into_iter()
        .skip(skip)
        .take(limit)
        .map(|p| {
            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            (p, size)
        })
        .collect();

    let parallel = cfg!(feature = "parallel");
    let total_bytes: u64 = files.iter().map(|(_, b)| *b).sum();
    println!(
        "scanbench: {} files, {:.1} MB source, parallel feature {}",
        files.len(),
        total_bytes as f64 / 1_048_576.0,
        if parallel { "ON" } else { "OFF" }
    );
    println!();

    for m in modes {
        match m {
            "seq" => {
                let (stats, slowest) = run_seq(&files, top_n);
                report("seq", &stats);
                if top_n > 0 && !slowest.is_empty() {
                    println!("top {} slowest files (Catalog::scan):", slowest.len());
                    for (rank, (nanos, size, path)) in slowest.iter().enumerate() {
                        println!(
                            "  {:2}. {:>8.3} s  {:>10} B  {}",
                            rank + 1,
                            *nanos as f64 / 1e9,
                            size,
                            path.display()
                        );
                    }
                    println!();
                }
            }
            "par" => report("par", &run_par(&files)),
            "par-canon" => report("par-canon", &run_par_canon(&files)),
            _ => unreachable!(),
        }
    }
}
