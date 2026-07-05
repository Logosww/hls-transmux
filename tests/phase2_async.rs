#![cfg(feature = "default-source")]

use std::fs;
use std::path::PathBuf;

use hls_transmux::{
    HlsInput, TrackType, TransmuxOptions, VariantSelection, transmux_hls_to_mp4_async,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// In-repo H.264 + AAC-LC transport stream fixture (single segment, ~10s).
fn fixture_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264_aac_fhd.ts")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hls-transmux-{name}-{}", std::process::id()));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn remuxes_local_master_playlist_with_explicit_variant() {
    let fixture = fixture_path();
    let dir = temp_dir("local-master");
    fs::create_dir_all(dir.join("low")).unwrap();
    fs::copy(&fixture, dir.join("low/segment.ts")).unwrap();
    fs::write(
        dir.join("master.m3u8"),
        "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"\nlow/media.m3u8\n",
    )
    .unwrap();
    fs::write(
        dir.join("low/media.m3u8"),
        "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXTINF:7,\nsegment.ts\n#EXT-X-ENDLIST\n",
    )
    .unwrap();

    let output = dir.join("output.mp4");
    let report = transmux_hls_to_mp4_async(
        HlsInput::Path(dir.join("master.m3u8")),
        &output,
        TransmuxOptions {
            variant: Some(VariantSelection::Index(0)),
            ..Default::default()
        },
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
}

#[tokio::test]
async fn remuxes_local_byterange_segment() {
    let fixture = fixture_path();
    let bytes = fs::read(&fixture).unwrap();
    let dir = temp_dir("local-byterange");
    fs::copy(&fixture, dir.join("segment.ts")).unwrap();
    fs::write(
        dir.join("media.m3u8"),
        format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXTINF:7,\n#EXT-X-BYTERANGE:{}@0\nsegment.ts\n#EXT-X-ENDLIST\n",
            bytes.len()
        ),
    )
    .unwrap();

    let output = dir.join("output.mp4");
    let report = transmux_hls_to_mp4_async(
        HlsInput::Path(dir.join("media.m3u8")),
        &output,
        TransmuxOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 1);
    assert!(report.bytes_written > 0);
}

#[tokio::test]
async fn remuxes_http_master_playlist_with_range_segment() {
    let fixture = fixture_path();
    let segment = fs::read(&fixture).unwrap();
    let server = TestServer::spawn(segment).await;

    let dir = temp_dir("http-master");
    let output = dir.join("output.mp4");
    let report = transmux_hls_to_mp4_async(
        HlsInput::Url(format!("{}/master.m3u8", server.base_url)),
        &output,
        TransmuxOptions {
            variant: Some(VariantSelection::Index(0)),
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 1);
    assert!(report.bytes_written > 0);
    assert!(output.exists());
}

struct TestServer {
    base_url: String,
}

impl TestServer {
    async fn spawn(segment: Vec<u8>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let segment = segment.clone();
                tokio::spawn(async move {
                    let mut buffer = vec![0_u8; 4096];
                    let Ok(size) = stream.read(&mut buffer).await else {
                        return;
                    };
                    let request = String::from_utf8_lossy(&buffer[..size]);
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("/");
                    let range = request.lines().find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("range")
                            .then(|| value.trim().strip_prefix("bytes="))
                            .flatten()
                    });
                    let (status, headers, body) = response_for(path, range, &segment);
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });

        Self { base_url }
    }
}

fn response_for(
    path: &str,
    range: Option<&str>,
    segment: &[u8],
) -> (&'static str, String, Vec<u8>) {
    match path {
        "/master.m3u8" => (
            "200 OK",
            "Content-Type: application/vnd.apple.mpegurl\r\n".to_owned(),
            b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia/playlist.m3u8\n".to_vec(),
        ),
        "/media/playlist.m3u8" => (
            "200 OK",
            "Content-Type: application/vnd.apple.mpegurl\r\n".to_owned(),
            format!(
                "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXTINF:7,\n#EXT-X-BYTERANGE:{}@0\nsegment.ts\n#EXT-X-ENDLIST\n",
                segment.len()
            )
            .into_bytes(),
        ),
        "/media/segment.ts" => {
            let Some(range) = range else {
                return (
                    "200 OK",
                    "Content-Type: video/mp2t\r\n".to_owned(),
                    segment.to_vec(),
                );
            };
            let (start, end) = range.split_once('-').unwrap();
            let start = start.parse::<usize>().unwrap();
            let end = end.parse::<usize>().unwrap();
            (
                "206 Partial Content",
                format!(
                    "Content-Type: video/mp2t\r\nContent-Range: bytes {start}-{end}/{}\r\n",
                    segment.len()
                ),
                segment[start..=end].to_vec(),
            )
        }
        _ => ("404 Not Found", String::new(), Vec::new()),
    }
}
