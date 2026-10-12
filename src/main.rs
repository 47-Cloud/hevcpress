mod analyze;
mod audio;
mod bits;
mod bmff;
mod cabac_enc;
mod codec;
mod coder;
mod cabac;
mod ctu;
mod model;
mod nal;
mod ps;
mod tables;

use std::env;
use std::fs;
use std::process;

fn inspect(path: &str) -> Result<(), String> {
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let ex = bmff::extract(&data)?;
    println!("{path}: {} bytes", data.len());
    nal::report(&data, &ex);
    Ok(())
}

fn syntax(path: &str, verbose: bool, model: &mut model::Model, cold: bool) -> Result<(), String> {
    if cold {
        *model = model::Model::new();
    }
    let data = fs::read(path).map_err(|e| e.to_string())?;
    let ex = bmff::extract(&data)?;
    println!("{path}: {} bytes", data.len());
    analyze::run(&data, &ex, verbose, model);
    Ok(())
}

fn read_file(path: &str) -> Result<Vec<u8>, String> {
    fs::read(path).map_err(|e| format!("{path}: {e}"))
}

fn verify(path: &str) -> Result<(), String> {
    let data = read_file(path)?;
    let t0 = std::time::Instant::now();
    let (comp, report) = codec::compress(&data, true);
    let secs = t0.elapsed().as_secs_f64();
    println!(
        "{path}: {} -> {} bytes ({:.2}% smaller), {}/{} slices recompressed, {:.1}s",
        data.len(),
        comp.len(),
        100.0 * (1.0 - comp.len() as f64 / data.len() as f64),
        report.slices_coded,
        report.slices_total,
        secs
    );
    println!(
        "    skeleton {} bytes -> {} compressed; slice data stream {} bytes",
        report.skeleton_bytes, report.meta_bytes, report.stream_bytes
    );
    Ok(())
}

/// Write `bytes` to `path` via a temporary file, never replacing an existing file.
fn write_new(path: &str, bytes: &[u8]) -> Result<(), String> {
    if std::path::Path::new(path).exists() {
        return Err(format!("{path} already exists, not overwriting"));
    }
    let tmp = format!("{path}.partial");
    fs::write(&tmp, bytes).map_err(|e| format!("{tmp}: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("{path}: {e}"))
}

/// Run `job` over `files` on `jobs` threads; returns (input bytes, output bytes, failures).
fn run_parallel(files: &[String], jobs: usize, job: impl Fn(&str) -> Result<(usize, usize), String> + Sync) -> (u64, u64, usize) {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    let next = AtomicUsize::new(0);
    let totals = Mutex::new((0u64, 0u64, 0usize));
    std::thread::scope(|sc| {
        for _ in 0..jobs.max(1).min(files.len().max(1)) {
            sc.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some(f) = files.get(i) else { break };
                let r = job(f);
                let mut t = totals.lock().unwrap();
                match r {
                    Ok((a, b)) => {
                        t.0 += a as u64;
                        t.1 += b as u64;
                    }
                    Err(e) => {
                        eprintln!("{f}: {e}");
                        t.2 += 1;
                    }
                }
            });
        }
    });
    totals.into_inner().unwrap()
}

/// Parse `[-j N] files...`.
fn jobs_and_files(args: &[String]) -> (usize, Vec<String>) {
    let mut jobs = std::thread::available_parallelism().map_or(4, |n| n.get());
    let mut files = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "-j" && i + 1 < args.len() {
            jobs = args[i + 1].parse().unwrap_or(jobs);
            i += 2;
        } else {
            files.push(args[i].clone());
            i += 1;
        }
    }
    (jobs, files)
}

fn main() {
    let args: Vec<String> = env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("inspect") if args.len() > 2 => {
            for f in &args[2..] {
                if let Err(e) = inspect(f) {
                    eprintln!("{f}: {e}");
                    process::exit(1);
                }
                println!();
            }
        }
        Some("syntax") if args.len() > 2 => {
            let verbose = args.iter().any(|a| a == "-v");
            let cold = args.iter().any(|a| a == "--cold");
            let mut model = model::Model::new();
            for f in args[2..].iter().filter(|a| !a.starts_with('-')) {
                if let Err(e) = syntax(f, verbose, &mut model, cold) {
                    eprintln!("{f}: {e}");
                    process::exit(1);
                }
                println!();
            }
        }
        Some("compress") if args.len() == 4 => {
            let r = read_file(&args[2]).and_then(|d| {
                let (c, rep) = codec::compress(&d, true);
                eprintln!("{} -> {} bytes ({}/{} slices recompressed)", d.len(), c.len(), rep.slices_coded, rep.slices_total);
                fs::write(&args[3], c).map_err(|e| e.to_string())
            });
            if let Err(e) = r {
                eprintln!("{e}");
                process::exit(1);
            }
        }
        Some("decompress") if args.len() == 4 => {
            let r = read_file(&args[2]).and_then(|c| codec::decompress(&c)).and_then(|d| fs::write(&args[3], d).map_err(|e| e.to_string()));
            if let Err(e) = r {
                eprintln!("{e}");
                process::exit(1);
            }
        }
        Some("audiotest") if args.len() == 3 => {
            let raw = fs::read(&args[2]).expect("read");
            let samples: Vec<i16> = raw.chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]])).collect();
            let mut model = model::Model::new();
            let mut enc = coder::Encoder::new();
            audio::encode(&samples, 1, &mut model, &mut enc);
            let out = enc.finish();
            println!("{}: {} -> {} bytes ({:.1}%)", args[2], raw.len(), out.len(), 100.0 * out.len() as f64 / raw.len() as f64);
        }
        Some("pack") if args.len() > 2 => {
            // Compress each file to <file>.hvp, several files at a time.
            let (jobs, files) = jobs_and_files(&args[2..]);
            let t0 = std::time::Instant::now();
            let (a, b, failed) = run_parallel(&files, jobs, |f| {
                let out = format!("{f}.hvp");
                if std::path::Path::new(&out).exists() {
                    return Err(format!("{out} already exists, not overwriting"));
                }
                let data = read_file(f)?;
                let (comp, rep) = codec::compress(&data, true);
                write_new(&out, &comp)?;
                println!(
                    "{f}: {} -> {} bytes ({:.2}% smaller, {}/{} slices)",
                    data.len(),
                    comp.len(),
                    100.0 * (1.0 - comp.len() as f64 / data.len().max(1) as f64),
                    rep.slices_coded,
                    rep.slices_total
                );
                Ok((data.len(), comp.len()))
            });
            if a > 0 {
                println!(
                    "total: {a} -> {b} bytes ({:.2}% smaller), {} files, {failed} failed, {:.1}s",
                    100.0 * (1.0 - b as f64 / a as f64),
                    files.len(),
                    t0.elapsed().as_secs_f64()
                );
            } else {
                println!("total: nothing packed, {failed} failed");
            }
            if failed > 0 {
                process::exit(1);
            }
        }
        Some("unpack") if args.len() > 2 => {
            // Restore each <file>.hvp to <file>.
            let (jobs, files) = jobs_and_files(&args[2..]);
            let (_, _, failed) = run_parallel(&files, jobs, |f| {
                let out = f.strip_suffix(".hvp").ok_or("expected a .hvp file")?;
                if std::path::Path::new(out).exists() {
                    return Err(format!("{out} already exists, not overwriting"));
                }
                let data = codec::decompress(&read_file(f)?)?;
                write_new(out, &data)?;
                println!("{f} -> {out} ({} bytes)", data.len());
                Ok((0, 0))
            });
            if failed > 0 {
                process::exit(1);
            }
        }
        Some("verify") if args.len() > 2 => {
            for f in &args[2..] {
                if let Err(e) = verify(f) {
                    eprintln!("{e}");
                    process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: hevcpress pack|unpack [-j N] files... | compress|decompress in out | verify|inspect|syntax files...");
            process::exit(2);
        }
    }
}
