//! Round-trip every file in $HEVCPRESS_SAMPLES (if set) through the binary.
//! Point it at a folder of your own HEIC / MOV files:
//!     HEVCPRESS_SAMPLES=~/Pictures/test cargo test --release -- --nocapture

use std::process::Command;

#[test]
fn samples_round_trip() {
    let Ok(dir) = std::env::var("HEVCPRESS_SAMPLES") else {
        eprintln!("HEVCPRESS_SAMPLES not set, skipping");
        return;
    };
    let bin = env!("CARGO_BIN_EXE_hevcpress");
    let tmp = std::env::temp_dir().join(format!("hevcpress-test-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if !path.is_file() {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let (packed, restored) = (tmp.join(format!("{name}.hvp")), tmp.join(&name));
        let ok = Command::new(bin)
            .args(["compress"])
            .arg(&path)
            .arg(&packed)
            .status()
            .unwrap();
        assert!(ok.success(), "compress failed: {name}");
        let ok = Command::new(bin)
            .args(["decompress"])
            .arg(&packed)
            .arg(&restored)
            .status()
            .unwrap();
        assert!(ok.success(), "decompress failed: {name}");
        let (a, b) = (std::fs::read(&path).unwrap(), std::fs::read(&restored).unwrap());
        assert!(a == b, "round trip changed {name}");
        let size = std::fs::metadata(&packed).unwrap().len();
        eprintln!("{name}: {} -> {size} bytes", a.len());
        checked += 1;
    }
    std::fs::remove_dir_all(&tmp).ok();
    eprintln!("{checked} files round-tripped");
}
