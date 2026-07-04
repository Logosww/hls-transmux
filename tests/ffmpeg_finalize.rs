#![cfg(feature = "ffmpeg-finalize")]

use std::fs;
use std::path::PathBuf;

use hls_transmux::{
    FinalizeBackend, HlsInput, OutputFormat, TransmuxOptions, transmux_hls_to_mp4_async,
};

/// In-repo H.264 + AAC-LC transport stream fixture (single segment, ~10s).
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264_aac_fhd.ts")
}

#[tokio::test]
async fn streaming_mp4_ffmpeg_finalize_remuxes_ts_fixture() {
    let fixture = fixture_path();

    let temp_dir = std::env::temp_dir().join(format!(
        "hls-transmux-ffmpeg-test-{}",
        std::process::id()
    ));
    fs::create_dir_all(&temp_dir).unwrap();
    let segment = temp_dir.join("segment.ts");
    let playlist = temp_dir.join("playlist.m3u8");
    let output = temp_dir.join("output_ffmpeg.mp4");

    fs::copy(&fixture, &segment).unwrap();
    fs::write(
        &playlist,
        "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:7.0,\nsegment.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();

    let report = transmux_hls_to_mp4_async(
        HlsInput::Path(playlist),
        &output,
        TransmuxOptions {
            output_format: OutputFormat::StreamingMp4,
            finalize_backend: FinalizeBackend::Ffmpeg,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 1);
    assert!(report.bytes_written > 0);
    assert!(output.exists());

    let output_bytes = fs::read(&output).unwrap();
    assert_eq!(&output_bytes[4..8], b"ftyp", "ftyp box missing");
    // ffmpeg is invoked with `movflags=faststart`, so moov should sit before
    // mdat. Sanity-check the layout and that the output is non-fragmented.
    let moov_pos = window_find(&output_bytes, b"moov");
    let mdat_pos = window_find(&output_bytes, b"mdat");
    assert!(moov_pos.is_some(), "moov box missing");
    assert!(mdat_pos.is_some(), "mdat box missing");
    assert!(
        moov_pos.unwrap() < mdat_pos.unwrap(),
        "moov must precede mdat (faststart)"
    );
    assert!(
        !window_contains(&output_bytes, b"styp"),
        "output is still fragmented (styp present)"
    );
    assert!(
        !window_contains(&output_bytes, b"moof"),
        "output is still fragmented (moof present)"
    );

    // ffprobe cross-check (optional, only if ffprobe is on PATH).
    let probe = std::process::Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        .arg("-show_entries")
        .arg("format=duration:stream=codec_type,codec_name")
        .arg("-of")
        .arg("default=noprint_wrappers=1")
        .arg(&output)
        .output();
    if let Ok(out) = probe {
        if out.status.success() {
            let stdout = String::from_utf8_lossy(&out.stdout);
            println!("ffprobe output:\n{stdout}");
            assert!(stdout.contains("codec_type=video"));
            assert!(stdout.contains("codec_type=audio"));
        } else {
            eprintln!(
                "ffprobe failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }

    let _ = fs::remove_dir_all(&temp_dir);
}

fn window_find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn window_contains(haystack: &[u8], needle: &[u8]) -> bool {
    window_find(haystack, needle).is_some()
}

