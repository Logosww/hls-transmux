//! WASM-compatible HLS → MP4 transmux demo.
//!
//! Demonstrates the exact same API pattern a browser / wasm consumer would use:
//!   1. Pre-fetch playlist text + segment bytes (here: from local files;
//!      in a browser: via `fetch()`)
//!   2. Load into [`MemorySource`]
//!   3. Transmux to classic MP4 bytes via [`transmux_hls_to_mp4_bytes`]
//!   4. Also show FragmentedMp4 via [`transmux_hls_to_writer_async`]
//!
//! The transmux step (MemorySource → bytes) uses the **same code path** on
//! native and `wasm32-unknown-unknown`. Only the pre-fetch step differs:
//!
//! | Platform | Pre-fetch | Transmux |
//! | -------- | --------- | -------- |
//! | Native   | `std::fs::read` | `transmux_hls_to_mp4_bytes` |
//! | Browser  | `fetch()` → `MemorySource` | `transmux_hls_to_mp4_bytes` |
//!
//! Run with:
//!     cargo run --example wasm_demo -- <playlist.m3u8> [output.mp4]
//!
//! Arguments:
//!     playlist.m3u8 = local media playlist path (required)
//!     output.mp4    = output file path (default: ./wasm_demo_output.mp4)
//!
//! The playlist must be a **media playlist** (not a master playlist).
//! Segment URIs are resolved relative to the playlist file location.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use hls_transmux::{
    HlsInput, MemorySource, OutputFormat, SourceLocation, TransmuxOptions,
    transmux_hls_to_mp4_bytes, transmux_hls_to_writer_async,
};

/// Parses a media playlist string and returns the segment URI list.
///
/// This is a simplified parser for demo purposes — any non-comment,
/// non-empty line is treated as a segment URI.
fn parse_segment_uris(playlist_text: &str) -> Vec<String> {
    playlist_text
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .collect()
}

/// Pre-fetches playlist text and segment bytes from the local filesystem.
///
/// In a browser, this step would use `fetch()` to load the playlist and
/// each segment, then hand the bytes to `MemorySource`.
fn prefetch_from_disk(
    playlist_path: &Path,
) -> hls_transmux::Result<(String, MemorySource)> {
    // Read playlist text.
    let playlist_text = std::fs::read_to_string(playlist_path)
        .map_err(|e| hls_transmux::Error::invalid(format!(
            "failed to read playlist '{}': {e}", playlist_path.display()
        )))?;

    // Parse segment URIs and resolve them relative to the playlist location.
    let segment_uris = parse_segment_uris(&playlist_text);
    let base_dir = playlist_path.parent().unwrap_or_else(|| Path::new(""));

    let playlist_key = playlist_path.to_string_lossy().into_owned();
    let mut source = MemorySource::new();
    source = source.text(&playlist_key, playlist_text.clone());

    for uri in &segment_uris {
        let segment_path = base_dir.join(uri);
        let segment_bytes = std::fs::read(&segment_path)
            .map_err(|e| hls_transmux::Error::invalid(format!(
                "failed to read segment '{}': {e}", segment_path.display()
            )))?;
        let seg_key = segment_path.to_string_lossy().into_owned();
        source = source.segment(&seg_key, segment_bytes);
    }

    println!("Pre-fetched {} segments from {}", segment_uris.len(), playlist_path.display());
    Ok((playlist_text, source))
}

#[tokio::main]
async fn main() -> hls_transmux::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let playlist_path = args.first()
        .ok_or_else(|| hls_transmux::Error::unsupported(
            "usage: cargo run --example wasm_demo -- <playlist.m3u8> [output.mp4]"
        ))?;
    let playlist_path = PathBuf::from(playlist_path);

    let output_path = args.get(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| "wasm_demo_output.mp4".to_string());

    println!("=== WASM-compatible HLS → MP4 transmux demo ===");
    println!();

    // --- Step 1: Pre-fetch (simulates browser fetch) ---
    println!("[1] Pre-fetching playlist + segments from disk...");
    let (_, source) = prefetch_from_disk(&playlist_path)?;
    println!();

    // Build HlsInput::custom with MemorySource.
    // In a browser, this would be SourceLocation::Url(playlist_url).
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path.clone()),
    );

    // --- Step 2a: Classic MP4 via transmux_hls_to_mp4_bytes (R2 API) ---
    println!("[2a] Transmuxing to classic MP4 (transmux_hls_to_mp4_bytes)...");
    let (mp4_bytes, report) = transmux_hls_to_mp4_bytes(
        input.clone(),
        TransmuxOptions::default(), // OutputFormat::Mp4 (classic, in-memory)
    ).await?;

    println!("  segments processed : {}", report.segment_count);
    println!("  bytes written      : {}", mp4_bytes.len());
    println!("  duration (ms)      : {}", report.duration);
    for track in &report.tracks {
        println!(
            "  track: {:?} {:?} {} samples, {}x{}@{}Hz",
            track.track_type,
            track.codec,
            track.sample_count,
            track.width.unwrap_or(0),
            track.height.unwrap_or(0),
            track.sample_rate.unwrap_or(0),
        );
    }
    println!();

    // Write classic MP4 to disk.
    std::fs::write(&output_path, &mp4_bytes)
        .map_err(|e| hls_transmux::Error::invalid(format!(
            "failed to write output '{}': {e}", output_path
        )))?;
    println!("  → wrote classic MP4 to: {output_path}");
    println!();

    // --- Step 2b: Fragmented MP4 via transmux_hls_to_writer_async (R3 API) ---
    println!("[2b] Transmuxing to fragmented MP4 (transmux_hls_to_writer_async)...");
    let mut fmp4_buf: Vec<u8> = Vec::new();
    let fmp4_report = transmux_hls_to_writer_async(
        input,
        &mut fmp4_buf,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    ).await?;

    println!("  segments processed : {}", fmp4_report.segment_count);
    println!("  bytes written      : {}", fmp4_buf.len());
    println!("  duration (ms)      : {}", fmp4_report.duration);
    println!();

    let fmp4_path = format!("{}.fmp4", output_path);
    std::fs::write(&fmp4_path, &fmp4_buf)
        .map_err(|e| hls_transmux::Error::invalid(format!(
            "failed to write fmp4 output '{}': {e}", fmp4_path
        )))?;
    println!("  → wrote fragmented MP4 to: {fmp4_path}");
    println!();

    println!("=== Done. Verify with: ffprobe {} ===", output_path);

    Ok(())
}
