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
        Some("verify") if args.len() > 2 => {
            for f in &args[2..] {
                if let Err(e) = verify(f) {
                    eprintln!("{e}");
                    process::exit(1);
                }
            }
        }
        _ => {
            eprintln!("usage: hevcpress inspect|syntax|compress|decompress|verify ...");
            process::exit(2);
        }
    }
}
