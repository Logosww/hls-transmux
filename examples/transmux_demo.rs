//! Minimal demo: transmux an HLS playlist to a local MP4 file.
//!
//! Run with:
//!     cargo run --example transmux_demo -- <playlist-url-or-path> [output] [variant-index] [flags]
//!
//! Arguments:
//!     playlist-url-or-path = HLS playlist URL (http/https) or local path (required)
//!     output               = ./output.mp4 (or ./output.fmp4 with --fragmented)
//!     variant-index        = optional positional (backward compat; overridden by --variant).
//!                           When absent, defaults to `HighestBandwidth`.
//!
//! Flags:
//!     --fragmented      = write fragmented MP4 (ftyp + moov + moof/mdat per segment)
//!     --streaming       = alias for the default `StreamingMp4` pipeline (no-op;
//!                         kept for backwards-compatible CLI usage)
//!     --ffmpeg-finalize = with --streaming, finalize via ffmpeg instead of the
//!                         built-in defrag path. Requires `--features ffmpeg-finalize`.
//!     --concurrency <n> = concurrent segment prefetch (default 4;
//!                         only effective for HTTP URLs). Requires `default-source`.
//!     --variant <v>     = variant selection: `highest`, `lowest`, or a numeric
//!                         index (overrides positional variant-index).

use hls_transmux::{
    FinalizeBackend, HlsInput, OutputFormat, TransmuxOptions, VariantSelection,
    transmux_hls_to_mp4_async,
};
#[cfg(feature = "default-source")]
use {
    std::sync::Arc,
    hls_transmux::{ReqwestSource, SourceLocation},
};

#[tokio::main]
async fn main() -> hls_transmux::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Parse flags that take a value: --concurrency <n>, --variant <v>.
    // Concurrency defaults to 4 (only effective for HTTP URLs; local files
    // are always sequential regardless of this value).
    let mut concurrency: usize = 4;
    let mut variant_flag: Option<String> = None;
    let mut remaining: Vec<&String> = Vec::new();
    let mut iter = args.iter().peekable();
    while let Some(arg) = iter.next() {
        if arg == "--concurrency" {
            let n = iter
                .next()
                .ok_or_else(|| hls_transmux::Error::invalid("--concurrency requires a value"))?;
            concurrency = n
                .parse()
                .map_err(|_| hls_transmux::Error::invalid("--concurrency value must be a number"))?;
        } else if arg == "--variant" {
            let v = iter
                .next()
                .ok_or_else(|| hls_transmux::Error::invalid("--variant requires a value"))?;
            variant_flag = Some(v.clone());
        } else if arg == "--fragmented" || arg == "--streaming" || arg == "--ffmpeg-finalize" {
            remaining.push(arg);
        } else {
            remaining.push(arg);
        }
    }

    let fragmented = remaining.iter().any(|a| **a == "--fragmented");
    // `--streaming` is now a no-op alias (StreamingMp4 is the default).
    // Kept for backwards-compatible CLI usage; the flag is parsed but
    // intentionally unused.
    let _streaming = remaining.iter().any(|a| **a == "--streaming");
    let ffmpeg_finalize_flag = remaining.iter().any(|a| **a == "--ffmpeg-finalize");
    let positional: Vec<&String> = remaining
        .iter()
        .filter(|a| {
            **a != "--fragmented" && **a != "--streaming" && **a != "--ffmpeg-finalize"
        })
        .copied()
        .collect();

    let Some(url) = positional.first() else {
        eprintln!(
            "usage: cargo run --example transmux_demo -- <playlist-url-or-path> \
             [output] [variant-index] [--fragmented|--streaming] [--ffmpeg-finalize] \
             [--concurrency <n>] [--variant highest|lowest|<index>]"
        );
        eprintln!();
        eprintln!("<playlist-url-or-path> can be an http(s) URL or a local file path.");
        eprintln!("Examples:");
        eprintln!("    cargo run --example transmux_demo -- playlist.m3u8 out.mp4");
        eprintln!("    cargo run --example transmux_demo -- https://example.com/master.m3u8 out.mp4 0");
        eprintln!("    cargo run --example transmux_demo -- https://example.com/master.m3u8 out.mp4 --variant highest --concurrency 8");
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

    // Resolve variant selection: `--variant` flag takes precedence over the
    // positional `<variant-index>` (3rd arg, backward compat). When neither
    // is provided, default to `HighestBandwidth` (matches the common "best
    // quality" expectation for a demo).
    let variant: VariantSelection = if let Some(v) = &variant_flag {
        match v.as_str() {
            "highest" => VariantSelection::HighestBandwidth,
            "lowest" => VariantSelection::LowestBandwidth,
            s => VariantSelection::Index(
                s.parse()
                    .map_err(|_| hls_transmux::Error::invalid("--variant must be `highest`, `lowest`, or a numeric index"))?,
            ),
        }
    } else if let Some(idx) = positional.get(2) {
        VariantSelection::Index(
            idx.parse()
                .expect("variant-index must be a number"),
        )
    } else {
        VariantSelection::HighestBandwidth
    };

    // Output format selection. Default is `StreamingMp4` (streaming fMP4
    // pipeline + end defrag → standard MP4, lower peak memory); `--fragmented`
    // opts into fMP4 output directly; `--streaming` is kept as an explicit
    // alias for the default for backwards-compatible CLI usage.
    let format = if fragmented {
        OutputFormat::FragmentedMp4
    } else {
        OutputFormat::StreamingMp4
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

    let is_http = url.starts_with("http://") || url.starts_with("https://");

    // Build input. When --concurrency > 1 and input is an HTTP URL, use
    // ReqwestSource::with_concurrency + HlsInput::custom to enable bounded
    // concurrent segment prefetch. Otherwise fall back to the simple
    // HlsInput::Url / HlsInput::Path constructors (sequential).
    #[cfg(feature = "default-source")]
    let input = if is_http && concurrency > 1 {
        let source = ReqwestSource::with_concurrency(concurrency);
        let location = SourceLocation::Url(url::Url::parse(&url).map_err(|e| {
            hls_transmux::Error::invalid(format!("invalid URL: {e}"))
        })?);
        HlsInput::custom(Arc::new(source), location)
    } else if is_http {
        HlsInput::Url(url.clone())
    } else {
        if concurrency > 1 {
            eprintln!(
                "warning: --concurrency {concurrency} ignored for local file input \
                 (concurrent prefetch only applies to HTTP sources)"
            );
        }
        HlsInput::Path(std::path::PathBuf::from(url.clone()))
    };

    #[cfg(not(feature = "default-source"))]
    let input = {
        if concurrency > 1 {
            eprintln!(
                "warning: --concurrency {concurrency} ignored (built without `default-source` feature)"
            );
        }
        if is_http {
            HlsInput::Url(url.clone())
        } else {
            HlsInput::Path(std::path::PathBuf::from(url.clone()))
        }
    };

    println!("HLS         : {url}");
    println!("OUT         : {output}");
    println!("VARIANT     : {variant:?}");
    println!("FORMAT      : {format:?}");
    println!("FINALIZE    : {finalize_backend:?}");
    println!("CONCURRENCY : {concurrency}");
    println!();

    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            variant: Some(variant),
            output_format: format,
            finalize_backend,
            ..Default::default()
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
