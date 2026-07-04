//! Minimal demo: transmux an HLS playlist to a local MP4 file.
//!
//! Run with:
//!     cargo run --example transmux_demo -- <playlist-url-or-path> [output] [variant-index] [--fragmented|--streaming] [--ffmpeg-finalize]
//!
//! Arguments:
//!     playlist-url-or-path = HLS playlist URL (http/https) or local path (required)
//!     output               = ./output.mp4 (or ./output.fmp4 with --fragmented)
//!     variant-index        = 0
//!     --fragmented     = write fragmented MP4 (ftyp + moov + moof/mdat per segment)
//!     --streaming      = streaming fMP4 pipeline + finalize to standard MP4
//!                        (same output as default, lower memory, temp file on disk)
//!     --ffmpeg-finalize = with --streaming, finalize via ffmpeg instead of the
//!                        built-in defrag path. Requires `--features ffmpeg-finalize`.

use hls_transmux::{
    FinalizeBackend, HlsInput, OutputFormat, TransmuxOptions, VariantSelection,
    transmux_hls_to_mp4_async,
};

#[tokio::main]
async fn main() -> hls_transmux::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let fragmented = args.iter().any(|a| a == "--fragmented");
    let streaming = args.iter().any(|a| a == "--streaming");
    let ffmpeg_finalize_flag = args.iter().any(|a| a == "--ffmpeg-finalize");
    let positional: Vec<&String> = args
        .iter()
        .filter(|a| {
            *a != "--fragmented" && *a != "--streaming" && *a != "--ffmpeg-finalize"
        })
        .collect();

    let Some(url) = positional.first() else {
        eprintln!(
            "usage: cargo run --example transmux_demo -- <playlist-url-or-path> \
             [output] [variant-index] [--fragmented|--streaming] [--ffmpeg-finalize]"
        );
        eprintln!();
        eprintln!("<playlist-url-or-path> can be an http(s) URL or a local file path.");
        eprintln!("Examples:");
        eprintln!("    cargo run --example transmux_demo -- playlist.m3u8 out.mp4");
        eprintln!("    cargo run --example transmux_demo -- https://example.com/master.m3u8 out.mp4 0");
        return Err(hls_transmux::Error::unsupported(
            "missing required <playlist-url-or-path> argument",
        ));
    };
    let url = url.to_string();
    let output = positional
        .get(1)
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            if fragmented {
                "output.fmp4".to_string()
            } else {
                "output.mp4".to_string()
            }
        });
    let variant_index: usize = positional
        .get(2)
        .map(|s| s.parse().expect("variant-index must be a number"))
        .unwrap_or(0);

    let format = if fragmented {
        OutputFormat::FragmentedMp4
    } else if streaming {
        OutputFormat::StreamingMp4
    } else {
        OutputFormat::Mp4
    };

    #[cfg(feature = "ffmpeg-finalize")]
    let finalize_backend = if ffmpeg_finalize_flag {
        FinalizeBackend::Ffmpeg
    } else {
        FinalizeBackend::default()
    };
    #[cfg(not(feature = "ffmpeg-finalize"))]
    let finalize_backend = {
        if ffmpeg_finalize_flag {
            eprintln!(
                "warning: --ffmpeg-finalize ignored (built without `ffmpeg-finalize` feature); \
                 falling back to native defrag"
            );
        }
        FinalizeBackend::default()
    };

    // Pick `Path` for local inputs, `Url` for http(s) ones. This keeps the
    // demo usable for both offline fixtures and remote playlists.
    let input = if url.starts_with("http://") || url.starts_with("https://") {
        HlsInput::Url(url.clone())
    } else {
        HlsInput::Path(std::path::PathBuf::from(url.clone()))
    };

    println!("HLS        : {url}");
    println!("OUT        : {output}");
    println!("VAR        : {variant_index}");
    println!("FORMAT     : {format:?}");
    println!("FINALIZE   : {finalize_backend:?}");
    println!();

    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            variant: Some(VariantSelection::Index(variant_index)),
            output_format: format,
            finalize_backend,
        },
    )
    .await?;

    println!("Done.");
    println!("  segments processed : {}", report.segment_count);
    println!("  bytes written      : {}", report.bytes_written);
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

    Ok(())
}
