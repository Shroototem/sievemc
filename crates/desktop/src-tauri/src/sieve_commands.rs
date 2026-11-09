//! Tauri commands bridging the desktop UI to the `sievemc` core library.
//!
//! - [`scan`] walks a mods directory, reads each jar's manifest in parallel via
//!   [`sievemc::detect_mod`] (streaming `sieve://scan-progress` events so the
//!   UI can render a live tally + log), then resolves sides across the whole
//!   folder with [`sievemc::apply_dependency_sides`].
//! - [`export`] copies/zips the reviewed set through [`sievemc::export_mods`].
//! - [`open_path`] reveals an output folder in the OS file manager.

use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use tauri::{AppHandle, Emitter};
use tauri_plugin_opener::OpenerExt;
use tracing::{info, warn};

use sievemc::{ExportItem, ExportReceipt, ExportSide, ModDetection, OutputType, Side};

/// Emitted once per jar as a scan progresses (channel: `sieve://scan-progress`).
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ScanProgress {
    /// 1-based index of the jar just processed.
    index: usize,
    total: usize,
    filename: String,
    side: Option<Side>,
    evidence: String,
    // Running tallies for the four buckets shown on the scan screen.
    client: usize,
    server: usize,
    both: usize,
    unsure: usize,
}

/// Scan `directory` for `*.jar` mods, classifying each one.
///
/// Emits a [`ScanProgress`] event after every jar and returns the full list of
/// detections (including "unsure" ones with `side: null`) when finished.
#[tauri::command]
pub async fn scan(app: AppHandle, directory: String) -> Result<Vec<ModDetection>, String> {
    let dir = PathBuf::from(&directory);
    info!("Scan requested for {}", dir.display());

    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|e| format!("Could not read directory: {e}"))?
        .filter_map(|f| f.ok().map(|e| e.path()))
        .filter(|p| {
            p.is_file()
                && p.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("jar"))
                    .unwrap_or(false)
        })
        .collect();
    files.sort();

    let total = files.len();
    // First pass: read each jar's manifest, in parallel (one jar per core).
    // Progress events fire as jars finish, so `index` counts completed jars.
    let done = AtomicUsize::new(0);
    let tally: [AtomicUsize; 4] = Default::default(); // client, server, both, unsure
    let mut results: Vec<ModDetection> = files
        .par_iter()
        .map(|file| {
            let detection = sievemc::detect_mod(file);
            let bucket = match detection.side {
                Some(Side::ClientOnly) => 0,
                Some(Side::ServerOnly) => 1,
                Some(Side::ClientAndServer) => 2,
                // Only dependency resolution (below) produces Unused.
                Some(Side::Unused) | None => 3,
            };
            tally[bucket].fetch_add(1, Ordering::Relaxed);
            let index = done.fetch_add(1, Ordering::Relaxed) + 1;
            let count = |i: usize| tally[i].load(Ordering::Relaxed);
            if let Err(e) = app.emit(
                "sieve://scan-progress",
                ScanProgress {
                    index,
                    total,
                    filename: detection.filename.clone(),
                    side: detection.side.clone(),
                    evidence: detection.evidence.clone(),
                    client: count(0),
                    server: count(1),
                    both: count(2),
                    unsure: count(3),
                },
            ) {
                warn!("Failed to emit scan-progress: {e}");
            }
            detection
        })
        .collect();

    // Second pass: propagate sides across the dependency graph (a library
    // required by server-side code must ship on the server too).
    sievemc::apply_dependency_sides(&mut results);
    let (mut client, mut server, mut both, mut unused, mut unsure) = (0usize, 0, 0, 0, 0);
    for d in &results {
        match d.side {
            Some(Side::ClientOnly) => client += 1,
            Some(Side::ServerOnly) => server += 1,
            Some(Side::ClientAndServer) => both += 1,
            Some(Side::Unused) => unused += 1,
            None => unsure += 1,
        }
    }

    info!(
        "Scan complete: {total} jars ({client} client, {server} server, {both} both, {unused} unused, {unsure} unsure)"
    );
    Ok(results)
}

/// A single mod with its effective (possibly user-overridden) side.
#[derive(serde::Deserialize)]
struct ExportItemDto {
    path: String,
    side: Side,
}

/// Payload for [`export`], sent from the Export page.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    items: Vec<ExportItemDto>,
    side: ExportSide,
    output_type: OutputType,
    dest_client: Option<String>,
    dest_server: Option<String>,
}

/// Export the reviewed mod set to disk, returning a receipt for the Done screen.
#[tauri::command]
pub async fn export(app: AppHandle, request: ExportRequest) -> Result<ExportReceipt, String> {
    let _ = app.emit("sieve://export-start", ());

    let items: Vec<ExportItem> = request
        .items
        .into_iter()
        .map(|i| ExportItem {
            path: PathBuf::from(i.path),
            side: i.side,
        })
        .collect();

    let receipt = sievemc::export_mods(
        &items,
        request.side,
        request.output_type,
        request.dest_client.map(PathBuf::from),
        request.dest_server.map(PathBuf::from),
    )
    .map_err(|e| e.to_string())?;

    let _ = app.emit("sieve://export-done", &receipt);
    Ok(receipt)
}

/// Reveal a path (usually an export folder) in the OS file manager.
#[tauri::command]
pub fn open_path(app: AppHandle, path: String) -> Result<(), String> {
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}
