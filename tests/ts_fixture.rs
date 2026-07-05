#![cfg(feature = "default-source")]

use std::fs;
use std::path::PathBuf;

use hls_transmux::{
    HlsInput, TrackType, TransmuxOptions, transmux_hls_to_mp4_async,
};

/// In-repo H.264 + AAC-LC transport stream fixture (single segment, ~10s).
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264_aac_fhd.ts")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hls-transmux-{name}-{}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn remuxes_h264_aac_ts_fixture_to_mp4() {
    let fixture = fixture_path();

    let dir = temp_dir("ts-fixture");
    let segment = dir.join("segment.ts");
    let playlist = dir.join("playlist.m3u8");
    let output = dir.join("output.mp4");

    fs::copy(&fixture, &segment).unwrap();
    fs::write(
        &playlist,
        "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:7.0,\nsegment.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();

    let report = transmux_hls_to_mp4_async(
        HlsInput::Path(playlist),
        &output,
        TransmuxOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 1);
    assert!(report.bytes_written > 0);
    assert!(output.exists());
    assert!(
        report
            .tracks
            .iter()
            .any(|track| track.track_type == TrackType::Video)
    );
    assert!(
        report
            .tracks
            .iter()
            .any(|track| track.track_type == TrackType::Audio)
    );

    let output_bytes = fs::read(&output).unwrap();
    assert_eq!(&output_bytes[4..8], b"ftyp");
}

