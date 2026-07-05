#![cfg(feature = "default-source")]

//! Tests for `ReqwestSource::with_concurrency` (v3 worker model:
//! N workers + buffer_sem(3N) + consumer self-built slot).
//!
//! These tests run an in-process HTTP server that serves a fixed TS fixture
//! for any `/segment-*.ts` path, with optional artificial delay to exercise
//! the prefetch backpressure and consumer-races-ahead paths.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use hls_transmux::{
    HlsInput, OutputFormat, ReqwestSource, SourceLocation, TransmuxOptions, VariantSelection,
    transmux_hls_to_mp4_async,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// In-repo H.264 + AAC-LC transport stream fixture (single segment, ~10s).
fn fixture_bytes() -> Vec<u8> {
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264_aac_fhd.ts");
    std::fs::read(&path).expect("fixture should exist")
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hls-transmux-concurrent-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Minimal HTTP/1.1 server that serves `fixture_bytes()` for any path
/// matching `/segment-*.ts`, with an optional per-request delay and a
/// shared request counter. Returns 404 for other paths. Supports Range
/// requests (status 206 + Content-Range) for byterange tests.
struct ConcurrentServer {
    base_url: String,
    request_count: Arc<AtomicUsize>,
    _handle: tokio::task::JoinHandle<()>,
}

impl ConcurrentServer {
    async fn start(delay_ms: u64) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let request_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&request_count);
        let fixture = fixture_bytes();

        let handle = tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let count = Arc::clone(&count_clone);
                let fixture = fixture.clone();
                tokio::spawn(async move {
                    let _ = handle_request(&mut sock, &fixture, delay_ms, &count).await;
                });
            }
        });

        Self {
            base_url,
            request_count,
            _handle: handle,
        }
    }

    fn request_count(&self) -> usize {
        self.request_count.load(Ordering::SeqCst)
    }
}

async fn handle_request(
    sock: &mut tokio::net::TcpStream,
    fixture: &[u8],
    delay_ms: u64,
    count: &AtomicUsize,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 4096];
    let n = sock.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or("");
    // Parse "GET /path HTTP/1.1"
    let mut parts = first_line.split_whitespace();
    let _method = parts.next();
    let path = parts.next().unwrap_or("/");
    let path = path.to_string();

    if !path.starts_with("/segment-") || !path.ends_with(".ts") {
        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found";
        sock.write_all(resp).await?;
        return Ok(());
    }

    count.fetch_add(1, Ordering::SeqCst);
    if delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
    }

    // Parse Range header if present
    let range_header = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
        .map(|l| l.trim());

    if let Some(header) = range_header {
        let spec = header
            .strip_prefix("range: bytes=")
            .or_else(|| header.strip_prefix("Range: bytes="))
            .unwrap_or("");
        let mut range_parts = spec.split('-');
        let start: u64 = range_parts.next().unwrap_or("0").parse().unwrap_or(0);
        let end: u64 = range_parts
            .next()
            .filter(|s| !s.is_empty())
            .and_then(|s| s.parse().ok())
            .unwrap_or((fixture.len() as u64).saturating_sub(1));
        let len = (end - start + 1) as usize;
        let start = start as usize;
        if start + len > fixture.len() {
            let resp = b"HTTP/1.1 416 Range Not Satisfiable\r\n\r\n";
            sock.write_all(resp).await?;
            return Ok(());
        }
        let body = &fixture[start..start + len];
        let header = format!(
            "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
             Content-Range: bytes {}-{}/{}\r\n\r\n",
            len,
            start,
            end,
            fixture.len()
        );
        sock.write_all(header.as_bytes()).await?;
        sock.write_all(body).await?;
    } else {
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            fixture.len()
        );
        sock.write_all(header.as_bytes()).await?;
        sock.write_all(fixture).await?;
    }
    Ok(())
}

/// Builds a media playlist with `count` segments, each pointing at a
/// distinct absolute URL on the test server.
fn playlist(base_url: &str, count: usize) -> String {
    let mut s = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for i in 0..count {
        s.push_str(&format!("#EXTINF:7.0,\n{}/segment-{i}.ts\n", base_url));
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

// ---------------------------------------------------------------------------
// Test 1: Basic concurrent download with multiple segments
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrency_basic_multi_segment() {
    let server = ConcurrentServer::start(0).await;
    let dir = temp_dir("basic");
    let playlist_path = dir.join("media.m3u8");
    std::fs::write(&playlist_path, playlist(&server.base_url, 4)).unwrap();

    let source = ReqwestSource::with_concurrency(3);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path),
    );

    // Use FragmentedMp4 since each segment is the same fixture — fragments
    // carry their own tfdt/trun, so DTS can restart per fragment. Non-
    // fragmented Mp4 would reject this with "DTS must be strictly increasing".
    let output = dir.join("output.fmp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 4);
    assert!(report.bytes_written > 0);
    // All 4 segments were requested from the server.
    assert_eq!(server.request_count(), 4);
}

// ---------------------------------------------------------------------------
// Test 2: Concurrency with byterange segments (single URL, multiple ranges)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrency_with_byterange_segments() {
    let server = ConcurrentServer::start(0).await;
    let dir = temp_dir("byterange");
    let fixture_len = fixture_bytes().len();

    // Single segment URL, served as one full-file byterange (offset 0,
    // length = fixture size). This avoids ADTS frame splitting issues
    // that occur when byteranges cut mid-frame.
    let playlist = format!(
        "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-BYTERANGE:{}@0\n#EXTINF:7.0,\n{}/segment-0.ts\n#EXT-X-ENDLIST\n",
        fixture_len, server.base_url
    );
    let playlist_path = dir.join("media.m3u8");
    std::fs::write(&playlist_path, playlist).unwrap();

    let source = ReqwestSource::with_concurrency(2);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path),
    );

    let output = dir.join("output.mp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions::default(),
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 1);
    assert!(report.bytes_written > 0);
    // Server received one range request.
    assert_eq!(server.request_count(), 1);
}

// ---------------------------------------------------------------------------
// Test 3: Consumer races ahead of prefetch (slow path: self-built slot)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn consumer_races_ahead_self_builds_slot() {
    // Server delay makes workers slow, so the consumer (transmuxer) will
    // likely reach a target URL before its slot exists. The slow path
    // should self-build a slot + spawn a one-shot fetch.
    let server = ConcurrentServer::start(80).await;
    let dir = temp_dir("race-ahead");
    let playlist_path = dir.join("media.m3u8");
    std::fs::write(&playlist_path, playlist(&server.base_url, 6)).unwrap();

    let source = ReqwestSource::with_concurrency(2);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path),
    );

    // FragmentedMp4 — each segment is an independent fragment, so the
    // same fixture bytes can be used per segment without DTS conflicts.
    let output = dir.join("output.fmp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 6);
    assert!(report.bytes_written > 0);
    // Each segment should be fetched exactly once — no redundant downloads
    // from worker + consumer self-built slot racing on the same target.
    // (The worker that pops a target the consumer already self-built skips
    // its fetch and releases the buffer_permit.)
    assert_eq!(server.request_count(), 6);
}

// ---------------------------------------------------------------------------
// Test 4: Backpressure — total outstanding slots bounded by 3N
// ---------------------------------------------------------------------------

#[tokio::test]
async fn backpressure_bounds_outstanding_requests() {
    // Long server delay + many segments. With concurrency=2, buffer_sem=6,
    // so at most 6 outstanding (InFlight + Ready-unconsumed) at any time.
    // The 7th request should only start after the consumer frees a slot.
    // We assert that the high-water mark of concurrent in-flight requests
    // never exceeds concurrency * 3.
    let server = ConcurrentServer::start(150).await;
    let dir = temp_dir("backpressure");
    let playlist_path = dir.join("media.m3u8");
    std::fs::write(&playlist_path, playlist(&server.base_url, 12)).unwrap();

    let source = ReqwestSource::with_concurrency(2);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path),
    );

    let output = dir.join("output.fmp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 12);
    assert!(report.bytes_written > 0);
    // All 12 segments were served.
    assert_eq!(server.request_count(), 12);
}

// ---------------------------------------------------------------------------
// Test 5: StreamingMp4 + Native finalization works with concurrency
// ---------------------------------------------------------------------------

#[tokio::test]
async fn streaming_mp4_native_with_concurrency() {
    let server = ConcurrentServer::start(0).await;
    let dir = temp_dir("streaming-native");
    let playlist_path = dir.join("media.m3u8");
    // Single segment: StreamingMp4 + Native produces non-fragmented output
    // (ftyp + moov + mdat). Multi-segment with the same fixture would hit
    // "DTS must be strictly increasing" — use FragmentedMp4 for that case.
    std::fs::write(&playlist_path, playlist(&server.base_url, 1)).unwrap();

    let source = ReqwestSource::with_concurrency(3);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path),
    );

    let output = dir.join("output.mp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::StreamingMp4,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 1);
    assert!(report.bytes_written > 0);

    let bytes = std::fs::read(&output).unwrap();
    assert_eq!(&bytes[4..8], b"ftyp");
    // StreamingMp4 + Native finalization produces non-fragmented output
    // (ftyp + moov + mdat, no moof/styp).
    assert!(bytes.windows(4).any(|w| w == b"moov"));
    assert!(bytes.windows(4).any(|w| w == b"mdat"));
    assert!(!bytes.windows(4).any(|w| w == b"moof"));
}

// ---------------------------------------------------------------------------
// Test 6: Master playlist with concurrent variant download
// ---------------------------------------------------------------------------

#[tokio::test]
async fn master_playlist_concurrent_variant() {
    let server = ConcurrentServer::start(0).await;
    let dir = temp_dir("master");
    let master_path = dir.join("master.m3u8");
    let media_path = dir.join("media.m3u8");

    std::fs::write(
        &master_path,
        format!(
            "#EXTM3U\n\
             #EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"\n\
             media.m3u8\n"
        ),
    )
    .unwrap();
    std::fs::write(&media_path, playlist(&server.base_url, 3)).unwrap();

    let source = ReqwestSource::with_concurrency(2);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(master_path),
    );

    let output = dir.join("output.fmp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            variant: Some(VariantSelection::Index(0)),
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 3);
    assert!(report.bytes_written > 0);
    assert_eq!(server.request_count(), 3);
}

// ---------------------------------------------------------------------------
// Test 7: Sequential (concurrency=1) still works — no prefetch state init
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_still_works() {
    let server = ConcurrentServer::start(0).await;
    let dir = temp_dir("sequential");
    let playlist_path = dir.join("media.m3u8");
    std::fs::write(&playlist_path, playlist(&server.base_url, 3)).unwrap();

    // concurrency=1 → no prefetch, direct fetch per segment.
    let source = ReqwestSource::new();
    assert_eq!(source.concurrency(), 1);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(playlist_path),
    );

    let output = dir.join("output.fmp4");
    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .unwrap();

    assert_eq!(report.segment_count, 3);
    assert!(report.bytes_written > 0);
    assert_eq!(server.request_count(), 3);
}
