//! Print manifest vs dependency-resolved sides for a mods folder.
//! `cargo run --release --example resolve_sides -- <mods dir>`
use std::time::Instant;

fn main() {
    let dir = std::env::args()
        .nth(1)
        .expect("usage: resolve_sides <mods dir>");
    let t = Instant::now();
    let mods = sievemc::detect_mods(&dir).expect("readable directory");
    let elapsed = t.elapsed();
    for m in &mods {
        let side = |s: &Option<sievemc::Side>| match s {
            Some(sievemc::Side::ClientOnly) => "client",
            Some(sievemc::Side::ServerOnly) => "server",
            Some(sievemc::Side::ClientAndServer) => "both",
            Some(sievemc::Side::Unused) => "UNUSED",
            None => "unsure",
        };
        println!(
            "{:<45} manifest={:<7} resolved={:<7}{}",
            m.filename,
            side(&m.manifest_side),
            side(&m.side),
            if m.library { " [library]" } else { "" }
        );
        println!("    evidence: {}", m.evidence);
        for r in &m.required_by {
            println!(
                "    ← {:?} by {} via {:?}{}",
                r.side,
                r.by,
                r.kind,
                r.detail
                    .as_deref()
                    .map(|d| format!(": {d}"))
                    .unwrap_or_default()
            );
        }
        for w in &m.warnings {
            println!("    ! {w}");
        }
    }
    let input: Vec<_> = mods
        .iter()
        .map(|m| (m.path.clone(), m.manifest_side.clone()))
        .collect();
    let t = Instant::now();
    let _ = sievemc::resolve_sides(&input);
    println!("Total time (manifest scan + dependency resolution): {elapsed:?}");
    println!(
        "Dependency resolution alone (re-run, warm cache): {:?}",
        t.elapsed()
    );
}
