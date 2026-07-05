#![cfg(feature = "default-source")]

//! Tests for `ReqwestSource::with_headers` /
//! `ReqwestSource::with_concurrency_and_headers`: verify that custom request
//! headers (e.g. `Authorization: Bearer <token>`) reach the server on both
//! playlist `read_text` and segment `read_bytes` requests, on both the
//! sequential path and the v3 concurrent prefetch path.
//!
//! Strategy: run an in-process HTTP server that returns 401 for any request
//! missing the expected `Authorization` header. With headers set, transmux
//! succeeds; without, it fails with `Error::Http` mentioning 401.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use hls_transmux::{
    HlsInput, OutputFormat, ReqwestSource, SourceLocation, TransmuxOptions,
    transmux_hls_to_mp4_async,
};
use reqwest::header::{HeaderMap, HeaderValue, AUTHORIZATION};
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
        "hls-transmux-headers-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// The `Authorization` value tests must send to satisfy the server.
const EXPECTED_AUTH: &str = "Bearer test-token-123";

/// Builds a `HeaderMap` containing only the `Authorization` header.
fn auth_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_static(EXPECTED_AUTH),
    );
    headers
}

/// HTTP server that gates every request on `Authorization: Bearer test-token-123`.
/// - `GET /media.m3u8` → returns a 3-segment playlist with absolute URLs back
///   to this server.
/// - `GET /segment-N.ts` → returns the fixture bytes (200) or 206 for Range.
/// - Missing/wrong Authorization → 401.
///
/// `request_count` increments on every accepted connection (regardless of
/// auth result), so tests can assert the server was actually hit.
struct AuthServer {
    base_url: String,
    request_count: Arc<AtomicUsize>,
    _handle: tokio::task::JoinHandle<()>,
}

impl AuthServer {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let request_count = Arc::new(AtomicUsize::new(0));
        let count_clone = Arc::clone(&request_count);
        let fixture = fixture_bytes();

        let handle = {
            let base_url_for_task = base_url.clone();
            tokio::spawn(async move {
                loop {
                    let (mut sock, _) = match listener.accept().await {
                        Ok(s) => s,
                        Err(_) => return,
                    };
                    let count = Arc::clone(&count_clone);
                    let fixture = fixture.clone();
                    let base_url = base_url_for_task.clone();
                    tokio::spawn(async move {
                        let _ = handle_request(&mut sock, &fixture, &base_url, &count).await;
                    });
                }
            })
        };

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
    base_url: &str,
    count: &AtomicUsize,
) -> std::io::Result<()> {
    let mut buf = vec![0u8; 8192];
    let n = sock.read(&mut buf).await?;
    if n == 0 {
        return Ok(());
    }
    let request = String::from_utf8_lossy(&buf[..n]).to_string();
    count.fetch_add(1, Ordering::SeqCst);

    // Verify Authorization header.
    let auth_ok = request
        .lines()
        .any(|line| line.eq_ignore_ascii_case(&format!("Authorization: {EXPECTED_AUTH}")));
    if !auth_ok {
        let resp = b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 12\r\n\r\nunauthorized";
        sock.write_all(resp).await?;
        return Ok(());
    }

    let first_line = request.lines().next().unwrap_or("");
    let path = first_line
        .split_whitespace()
        .nth(1)
        .unwrap_or("/")
        .to_string();

    // Range header (for byterange segment requests).
    let range_header = request
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("range: bytes="))
        .map(|l| l.trim().to_string());

    if path == "/media.m3u8" {
        // 3-segment playlist with absolute URLs back to this server.
        let body = format!(
            "#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n\
             #EXTINF:7.0,\n{base_url}/segment-0.ts\n\
             #EXTINF:7.0,\n{base_url}/segment-1.ts\n\
             #EXTINF:7.0,\n{base_url}/segment-2.ts\n\
             #EXT-X-ENDLIST\n"
        );
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\n\
             Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        sock.write_all(resp.as_bytes()).await?;
    } else if path.starts_with("/segment-") && path.ends_with(".ts") {
        if let Some(range) = range_header.as_deref() {
            // Parse "Range: bytes=start-end"
            let spec = range
                .split('=')
                .nth(1)
                .and_then(|s| s.trim().split('-').next())
                .unwrap_or("0");
            let start: u64 = spec.parse().unwrap_or(0);
            let start = start as usize;
            let end = fixture.len().saturating_sub(1);
            let len = end - start + 1;
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
                "HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\n\
                 Content-Length: {}\r\n\r\n",
                fixture.len()
            );
            sock.write_all(header.as_bytes()).await?;
            sock.write_all(fixture).await?;
        }
    } else {
        let resp = b"HTTP/1.1 404 Not Found\r\nContent-Length: 9\r\n\r\nnot found";
        sock.write_all(resp).await?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Test 1: Sequential path — headers sent on playlist + segment requests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_headers_reach_server_on_playlist_and_segments() {
    let server = AuthServer::start().await;
    let dir = temp_dir("seq");
    let output = dir.join("output.fmp4");
    let playlist_url = format!("{}/media.m3u8", server.base_url);

    // Use HlsInput::Url so read_text goes through HTTP (exercises the
    // headers-on-read_text path). Concurrency=1 (sequential).
    let source = ReqwestSource::with_headers(auth_headers());
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::Url(
            url::Url::parse(&playlist_url).unwrap(),
        ),
    );

    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .expect("transmux should succeed when auth headers are set");

    assert_eq!(report.segment_count, 3);
    assert!(report.bytes_written > 0);
    // Server was hit at least 4 times: 1 playlist + 3 segments.
    assert!(
        server.request_count() >= 4,
        "server should be hit for playlist + segments, got {}",
        server.request_count()
    );
    assert!(output.exists());
}

// ---------------------------------------------------------------------------
// Test 2: Sequential path — without headers, server returns 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn sequential_without_headers_returns_401() {
    let server = AuthServer::start().await;
    let dir = temp_dir("seq-noauth");
    let output = dir.join("output.fmp4");
    let playlist_url = format!("{}/media.m3u8", server.base_url);

    // No headers → server should 401 on the playlist request.
    let source = ReqwestSource::new();
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::Url(url::Url::parse(&playlist_url).unwrap()),
    );

    let err = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .expect_err("should fail without auth headers");

    let msg = format!("{err}");
    assert!(
        msg.contains("401"),
        "error should mention 401, got: {msg}"
    );
    assert!(server.request_count() >= 1);
}

// ---------------------------------------------------------------------------
// Test 3: Concurrent path — headers propagated to v3 prefetch workers
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_headers_reach_workers() {
    let server = AuthServer::start().await;
    let dir = temp_dir("conc");
    let output = dir.join("output.fmp4");
    let playlist_url = format!("{}/media.m3u8", server.base_url);

    // Concurrency=3 + headers. Workers fetch segments with the auth header.
    let source = ReqwestSource::with_concurrency_and_headers(3, auth_headers());
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::Url(url::Url::parse(&playlist_url).unwrap()),
    );

    let report = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .expect("transmux should succeed when auth headers are set");

    assert_eq!(report.segment_count, 3);
    assert!(report.bytes_written > 0);
    // 1 playlist + 3 segment fetches (workers never retry on 401, so a
    // missing header would surface as Error::Http 401, not silent success).
    assert!(
        server.request_count() >= 4,
        "server should be hit for playlist + segments, got {}",
        server.request_count()
    );
}

// ---------------------------------------------------------------------------
// Test 4: Concurrent path without headers — workers hit 401
// ---------------------------------------------------------------------------

#[tokio::test]
async fn concurrent_without_headers_returns_401() {
    let server = AuthServer::start().await;
    let dir = temp_dir("conc-noauth");
    let output = dir.join("output.fmp4");
    let playlist_url = format!("{}/media.m3u8", server.base_url);

    // Concurrency=3, no headers → playlist request (read_text) returns 401
    // before workers even spawn (try_start_prefetch only fires after
    // read_text succeeds and returns a media playlist).
    let source = ReqwestSource::with_concurrency(3);
    let input = HlsInput::custom(
        Arc::new(source),
        SourceLocation::Url(url::Url::parse(&playlist_url).unwrap()),
    );

    let err = transmux_hls_to_mp4_async(
        input,
        &output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .expect_err("should fail without auth headers");

    let msg = format!("{err}");
    assert!(
        msg.contains("401"),
        "error should mention 401, got: {msg}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: headers() accessor returns the configured HeaderMap
// ---------------------------------------------------------------------------

#[tokio::test]
async fn headers_accessor_returns_configured_headers() {
    let mut headers = HeaderMap::new();
    headers.insert(AUTHORIZATION, HeaderValue::from_static("Bearer abc"));
    headers.insert("X-Custom", HeaderValue::from_static("custom-value"));

    let source = ReqwestSource::with_headers(headers);
    let retrieved = source.headers();
    assert_eq!(retrieved.len(), 2);
    assert_eq!(
        retrieved.get(AUTHORIZATION).unwrap(),
        HeaderValue::from_static("Bearer abc")
    );
    assert_eq!(
        retrieved.get("X-Custom").unwrap(),
        HeaderValue::from_static("custom-value")
    );

    // Default-constructed source has empty headers.
    let default_source = ReqwestSource::new();
    assert!(default_source.headers().is_empty());
}
