//! PHASE 4 tests: per-segment progress callback, cooperative cancellation,
//! and resume from checkpoint. Uses a mock `Source` that returns a fixed
//! multi-segment playlist backed by the in-repo H.264+AAC TS fixture.

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hls_transmux::{
    ByteRange, CancelToken, Error, HlsInput, OutputFormat, Source, SourceLocation, TextResource,
    TransmuxOptions, TransmuxProgress, TransmuxResumeState, transmux_hls_to_mp4_async,
};

/// Reads the in-repo H.264 + AAC-LC TS fixture bytes.
fn fixture_bytes() -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("h264_aac_fhd.ts");
    std::fs::read(&path).expect("fixture should exist")
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hls-transmux-phase4-{name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Builds a media playlist string with `count` segments. All segment URIs are
/// identical ("segment.ts") — the mock Source returns the same fixture bytes
/// regardless of which segment is requested.
fn playlist_with(count: usize) -> String {
    let mut s = String::from("#EXTM3U\n#EXT-X-TARGETDURATION:8\n#EXT-X-MEDIA-SEQUENCE:0\n");
    for _ in 0..count {
        s.push_str("#EXTINF:7.0,\nsegment.ts\n");
    }
    s.push_str("#EXT-X-ENDLIST\n");
    s
}

/// Mock `Source` that returns a fixed playlist text and fixed segment bytes.
/// Used to drive the transmux pipeline without real HTTP or filesystem I/O.
#[derive(Debug)]
struct MockSource {
    playlist: String,
    segment_bytes: Arc<Vec<u8>>,
}

impl Source for MockSource {
    fn read_text<'a>(
        &'a self,
        _location: &'a SourceLocation,
    ) -> Pin<Box<dyn Future<Output = hls_transmux::Result<TextResource>> + Send + 'a>> {
        let text = self.playlist.clone();
        Box::pin(async move {
            Ok(TextResource {
                content: text,
                location: SourceLocation::File(PathBuf::from("playlist.m3u8")),
            })
        })
    }

    fn read_bytes<'a>(
        &'a self,
        _location: &'a SourceLocation,
        _range: Option<&'a ByteRange>,
    ) -> Pin<Box<dyn Future<Output = hls_transmux::Result<Vec<u8>>> + Send + 'a>> {
        let bytes = (*self.segment_bytes).clone();
        Box::pin(async move { Ok(bytes) })
    }
}

/// Builds an `HlsInput::custom` backed by a `MockSource` with `segment_count`
/// segments, all returning the same fixture bytes.
fn mock_input(segment_count: usize) -> HlsInput {
    let source = MockSource {
        playlist: playlist_with(segment_count),
        segment_bytes: Arc::new(fixture_bytes()),
    };
    HlsInput::custom(
        Arc::new(source),
        SourceLocation::File(PathBuf::from("playlist.m3u8")),
    )
}

/// Test `CancelToken` backed by an `AtomicBool`. `cancelled()` returns a
/// pending future — tests exercise cancellation via `is_cancelled()` only.
#[derive(Debug, Default)]
struct TestCancelToken {
    flag: Arc<AtomicBool>,
}

impl TestCancelToken {
    fn trigger(&self) {
        self.flag.store(true, Ordering::SeqCst);
    }
}

impl CancelToken for TestCancelToken {
    fn is_cancelled(&self) -> bool {
        self.flag.load(Ordering::SeqCst)
    }

    fn cancelled(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(std::future::pending())
    }
}

/// Zeros out `creation_time` and `modification_time` fields in `mvhd` and
/// `mdhd` boxes within the `moov` box. These fields use wall-clock time
/// (`SystemTime::now()` in `mp4.rs`), so they differ between outputs
/// produced at different times. Normalizing them allows byte-level
/// comparison of structurally identical files.
fn normalize_moov_timestamps(data: &mut [u8]) {
    walk_and_zero_timestamps(data, 0, data.len());
}

fn walk_and_zero_timestamps(data: &mut [u8], start: usize, end: usize) {
    let mut offset = start;
    while offset + 8 <= end {
        let size = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        if size < 8 || offset + size > end {
            break;
        }
        let btype = &data[offset + 4..offset + 8];
        match btype {
            b"mvhd" | b"mdhd" => {
                // version(1) + flags(3) + creation_time(4) + modification_time(4)
                // For version 0, timestamps are 4 bytes each at offset 12..20.
                if offset + 20 <= end {
                    data[offset + 12..offset + 20].fill(0);
                }
            }
            b"moov" | b"trak" | b"mdia" => {
                walk_and_zero_timestamps(data, offset + 8, offset + size);
            }
            _ => {}
        }
        offset += size;
    }
}

// ---------------------------------------------------------------------------
// Test 1: progress callback fires once per segment (§9.2)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn progress_fires_per_segment() {
    let dir = temp_dir("progress");
    let output = dir.join("output.fmp4");

    let events: Arc<Mutex<Vec<TransmuxProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let events_cb = events.clone();

    let opts = TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        on_progress: Some(Arc::new(move |p: TransmuxProgress| {
            events_cb.lock().unwrap().push(p);
        })),
        ..Default::default()
    };

    transmux_hls_to_mp4_async(mock_input(3), &output, opts)
        .await
        .expect("transmux should succeed");

    let evs = events.lock().unwrap();
    assert_eq!(evs.len(), 3, "callback should fire once per segment");
    assert_eq!(evs[0].completed_segments, 1);
    assert_eq!(evs[1].completed_segments, 2);
    assert_eq!(evs[2].completed_segments, 3);
    assert_eq!(evs[2].total_segments, 3);
    // Monotonically non-decreasing downloaded_bytes / bytes_written.
    assert!(evs[1].downloaded_bytes >= evs[0].downloaded_bytes);
    assert!(evs[2].downloaded_bytes >= evs[1].downloaded_bytes);
    assert!(evs[1].bytes_written > evs[0].bytes_written);
    assert!(evs[2].bytes_written > evs[1].bytes_written);
    // current_segment_index tracks the just-completed segment.
    assert_eq!(evs[0].current_segment_index, 0);
    assert_eq!(evs[1].current_segment_index, 1);
    assert_eq!(evs[2].current_segment_index, 2);
    // Resume snapshot fields are populated.
    assert_eq!(evs[2].resume.completed_segments, 3);
    assert!(evs[2].resume.bytes_written > 0);
    assert!(evs[2].resume.next_sequence > 0);
}

// ---------------------------------------------------------------------------
// Test 2: cancel returns Error::Cancelled and retains .partial.mp4 (§9.3)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn cancel_returns_cancelled_error() {
    let dir = temp_dir("cancel");
    let output = dir.join("output.mp4");
    let partial = dir.join("output.partial.mp4");

    let token = Arc::new(TestCancelToken::default());
    let token_for_cb = token.clone();

    let opts = TransmuxOptions {
        output_format: OutputFormat::StreamingMp4,
        cancel: Some(token),
        on_progress: Some(Arc::new(move |p: TransmuxProgress| {
            // Trigger cancel after 2 segments complete. The next loop
            // iteration's check_cancel() will return Error::Cancelled.
            if p.completed_segments >= 2 {
                token_for_cb.trigger();
            }
        })),
        ..Default::default()
    };

    let err = transmux_hls_to_mp4_async(mock_input(3), &output, opts)
        .await
        .expect_err("should be cancelled");
    assert!(
        matches!(err, Error::Cancelled),
        "expected Error::Cancelled, got {err:?}"
    );

    // .partial.mp4 should be retained on cancel (StreamingMp4 stage 1 keeps
    // it so the caller can resume).
    assert!(
        partial.exists(),
        ".partial.mp4 should be retained on cancel"
    );
    let bytes = std::fs::read(&partial).expect("partial should be readable");
    assert!(
        bytes.len() >= 8 && &bytes[4..8] == b"ftyp",
        "partial file should start with ftyp box"
    );
}

// ---------------------------------------------------------------------------
// Test 3: resume produces byte-identical output (§9.4)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_produces_identical_output() {
    let dir = temp_dir("resume");
    let full_output = dir.join("full.fmp4");
    let resume_output = dir.join("resume.fmp4");

    // (a) Run to completion → C_full (includes mfra).
    transmux_hls_to_mp4_async(
        mock_input(3),
        &full_output,
        TransmuxOptions {
            output_format: OutputFormat::FragmentedMp4,
            ..Default::default()
        },
    )
    .await
    .expect("full run should succeed");
    let c_full = std::fs::read(&full_output).expect("full output should exist");

    // (b) Cancel after 2 segments → capture resume snapshot R.
    let token = Arc::new(TestCancelToken::default());
    let token_for_cb = token.clone();
    let snapshot: Arc<Mutex<Option<TransmuxResumeState>>> = Arc::new(Mutex::new(None));
    let snapshot_cb = snapshot.clone();

    let opts = TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        cancel: Some(token),
        on_progress: Some(Arc::new(move |p: TransmuxProgress| {
            *snapshot_cb.lock().unwrap() = Some(p.resume.clone());
            if p.completed_segments >= 2 {
                token_for_cb.trigger();
            }
        })),
        ..Default::default()
    };
    let err = transmux_hls_to_mp4_async(mock_input(3), &resume_output, opts)
        .await
        .expect_err("should be cancelled");
    assert!(matches!(err, Error::Cancelled));

    let r = snapshot
        .lock()
        .unwrap()
        .clone()
        .expect("should have a resume snapshot");
    assert_eq!(
        r.completed_segments, 2,
        "snapshot should record 2 completed segments"
    );

    // (c) Resume from R → C_resumed (omits mfra).
    let resumed_events: Arc<Mutex<Vec<TransmuxProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let re_cb = resumed_events.clone();
    let opts = TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        resume: Some(r),
        on_progress: Some(Arc::new(move |p: TransmuxProgress| {
            re_cb.lock().unwrap().push(p);
        })),
        ..Default::default()
    };
    transmux_hls_to_mp4_async(mock_input(3), &resume_output, opts)
        .await
        .expect("resume should succeed");
    let c_resumed = std::fs::read(&resume_output).expect("resumed output should exist");

    // (d) Normalize wall-clock timestamps, then compare byte-for-byte.
    // mvhd/mdhd creation_time and modification_time use SystemTime::now(), so
    // they differ between runs — normalize them to zero before byte comparison.
    // Both outputs include a complete mfra box: the resumed run rebuilds
    // historical tfra entries by scanning the existing .partial.mp4's moof
    // boxes (plan §5.5 enhancement), so the mfra boxes are byte-identical.
    let mut c_full_norm = c_full.clone();
    let mut c_resumed_norm = c_resumed.clone();
    normalize_moov_timestamps(&mut c_full_norm);
    normalize_moov_timestamps(&mut c_resumed_norm);
    assert_eq!(
        c_resumed_norm.as_slice(),
        c_full_norm.as_slice(),
        "resumed output should be byte-identical to full output (including \
         mfra, after wall-clock timestamp normalization)"
    );

    // (e) First (and only) resumed progress event: current_segment_index == K
    // (the resume start index), completed_segments == K + 1.
    let re = resumed_events.lock().unwrap();
    assert_eq!(
        re.len(),
        1,
        "resumed run processes 1 segment, should emit 1 progress event"
    );
    assert_eq!(re[0].current_segment_index, 2, "first resumed segment index");
    assert_eq!(re[0].completed_segments, 3, "completed_segments after resume");
}

// ---------------------------------------------------------------------------
// Test 4: resume boundary — already complete (§9.5)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn resume_boundary_already_complete() {
    let dir = temp_dir("resume-complete");
    let output = dir.join("output.fmp4");

    let opts = TransmuxOptions {
        output_format: OutputFormat::FragmentedMp4,
        resume: Some(TransmuxResumeState {
            completed_segments: 3, // == segments.len()
            bytes_written: 1000,
            next_sequence: 4,
            global_base_dts_90k: 0,
        }),
        ..Default::default()
    };

    let err = transmux_hls_to_mp4_async(mock_input(3), &output, opts)
        .await
        .expect_err("should reject already-complete resume");
    assert!(
        matches!(err, Error::InvalidInput(_)),
        "expected Error::InvalidInput, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 5: Mp4 + resume is rejected (user decision, §5.4 deviation)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn mp4_with_resume_rejected() {
    let dir = temp_dir("mp4-resume");
    let output = dir.join("output.mp4");

    let opts = TransmuxOptions {
        output_format: OutputFormat::Mp4,
        resume: Some(TransmuxResumeState {
            completed_segments: 1,
            bytes_written: 1000,
            next_sequence: 2,
            global_base_dts_90k: 0,
        }),
        ..Default::default()
    };

    let err = transmux_hls_to_mp4_async(mock_input(3), &output, opts)
        .await
        .expect_err("should reject Mp4 + resume");
    assert!(
        matches!(err, Error::InvalidInput(_)),
        "expected Error::InvalidInput, got {err:?}"
    );
}

// ---------------------------------------------------------------------------
// Test 6: default path (no hooks) is unchanged (§9.6)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn default_path_unchanged() {
    let dir = temp_dir("default");
    let output = dir.join("output.mp4");

    transmux_hls_to_mp4_async(
        mock_input(1),
        &output,
        TransmuxOptions::default(),
    )
    .await
    .expect("default path should succeed");

    let bytes = std::fs::read(&output).expect("output should exist");
    assert!(
        bytes.len() >= 8 && &bytes[4..8] == b"ftyp",
        "output should start with ftyp box"
    );
}
