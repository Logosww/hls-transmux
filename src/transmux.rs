use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::cancel::CancelToken;
use crate::codecs::avc;
use crate::error::{Error, Result};
use crate::hls::{HlsPlaylist, MasterPlaylist, MediaPlaylist, parse_hls_playlist_content};
use crate::isobmff::demux_isobmff;
use crate::mp4::{
    FragmentedMp4Muxer, FragmentedTrack, Mp4Muxer, Mp4Sample, TfraEntry, assign_delta_durations,
    make_audio_track, make_hevc_video_track, make_video_track, mfra_box,
};
use crate::mpeg_ts::demux_ts;
use crate::resume::TransmuxResumeState;
use crate::source::{HlsInput, SourceLocation, SourceReader};
use crate::types::{DemuxOutput, EncodedPacket, StreamKind, TransmuxReport};

/// Selects which variant of a master playlist to transmux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantSelection {
    /// Zero-based index into the master playlist's variant list.
    Index(usize),
    /// Select the variant with the highest `BANDWIDTH`. Variants without an
    /// explicit `BANDWIDTH` attribute are treated as 0. When multiple variants
    /// share the same (maximum) bandwidth, the last one in playlist order
    /// wins (Rust's `max_by_key` tie-breaking).
    HighestBandwidth,
    /// Select the variant with the lowest `BANDWIDTH`. Variants without an
    /// explicit `BANDWIDTH` attribute are treated as `u64::MAX`. When multiple
    /// variants share the same (minimum) bandwidth, the last one in playlist
    /// order wins (Rust's `min_by_key` tie-breaking).
    LowestBandwidth,
}

impl VariantSelection {
    /// Resolves `self` to a concrete zero-based index into
    /// `master.variants`, applying the selection strategy.
    pub(crate) fn select_index(&self, master: &MasterPlaylist) -> Result<usize> {
        match self {
            Self::Index(index) => {
                if *index >= master.variants.len() {
                    return Err(Error::invalid(format!(
                        "variant index {index} is out of range for {} variants",
                        master.variants.len()
                    )));
                }
                Ok(*index)
            }
            Self::HighestBandwidth => master
                .variants
                .iter()
                .enumerate()
                .max_by_key(|(_, v)| v.bandwidth.unwrap_or(0))
                .map(|(index, _)| index)
                .ok_or_else(|| Error::invalid("master playlist has no variants")),
            Self::LowestBandwidth => master
                .variants
                .iter()
                .enumerate()
                .min_by_key(|(_, v)| v.bandwidth.unwrap_or(u64::MAX))
                .map(|(index, _)| index)
                .ok_or_else(|| Error::invalid("master playlist has no variants")),
        }
    }
}

/// Output container format and pipeline.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OutputFormat {
    /// Standard non-fragmented MP4 (`ftyp` + `moov` + `mdat`).
    /// Batch pipeline: all segments are demuxed into memory before muxing.
    /// Fastest for small inputs; highest peak memory.
    #[default]
    Mp4,
    /// Fragmented MP4 / CMAF (`ftyp` + `moov` + per-segment `moof` + `mdat`).
    /// Streaming pipeline: each segment is written to disk as it's demuxed.
    /// Interruptible; produces fMP4 directly.
    FragmentedMp4,
    /// Standard non-fragmented MP4 via the streaming fragmented pipeline.
    /// Each segment is demuxed and written to a temp fMP4 file, then
    /// defragged into a single `ftyp` + `moov` + `mdat`. Output is identical
    /// to [`Mp4`](Self::Mp4), but peak memory is lower for long inputs. The
    /// temp file (`<output>.partial.<ext>`) is a playable fMP4 if the
    /// process is interrupted before finalization.
    StreamingMp4,
}

/// Backend used for the finalization step of [`OutputFormat::StreamingMp4`].
///
/// Ignored for other output formats.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FinalizeBackend {
    /// Self-contained defrag using the crate's own ISOBMFF demuxer + MP4
    /// muxer. No external dependencies. Produces faststart (`moov` before
    /// `mdat`) output. This is the default.
    #[default]
    Native,
    /// Use ffmpeg (via `ffmpeg-next`) to remux the temp fMP4 into a standard
    /// MP4. Requires the `ffmpeg-finalize` cargo feature and FFmpeg 8 shared
    /// libraries at build time. Useful when you want to defer to ffmpeg's
    /// battle-tested muxer or need ffmpeg-specific box layout.
    #[cfg(feature = "ffmpeg-finalize")]
    Ffmpeg,
}

/// Options for the async transmux entry point.
///
/// All fields have sensible defaults; `Default::default()` transmuxes to a
/// non-fragmented MP4 and requires the input to be a media playlist (master
/// playlists require an explicit [`VariantSelection`]).
///
/// # Progress, cancellation, resume
///
/// `on_progress`, `cancel`, and `resume` are all optional (default `None`).
/// When `None`, the pipeline behaves exactly as it did before these hooks
/// existed — existing callers and tests do not need to change.
///
/// - `on_progress`: invoked synchronously after each segment is fully
///   processed. The callback receives a [`TransmuxProgress`] which includes
///   a fresh [`TransmuxResumeState`] snapshot; callers should persist it if
///   they want to support resume.
/// - `cancel`: checked at the top of each segment iteration and raced
///   against `Source::read_bytes` await points. On cancel, the pipeline
///   returns [`Error::Cancelled`] promptly.
/// - `resume`: when `Some`, skips `segments[..completed_segments]` and
///   appends to the existing output file. Only supported for
///   [`OutputFormat::StreamingMp4`] and [`OutputFormat::FragmentedMp4`];
///   passing it with [`OutputFormat::Mp4`] returns [`Error::InvalidInput`].
#[derive(Clone, Default)]
pub struct TransmuxOptions {
    /// Required when the input is a master playlist; ignored for media playlists.
    pub variant: Option<VariantSelection>,
    /// Output container format / pipeline. See [`OutputFormat`].
    pub output_format: OutputFormat,
    /// Finalization backend for [`OutputFormat::StreamingMp4`]. Ignored for
    /// other formats. Default: [`FinalizeBackend::Native`].
    pub finalize_backend: FinalizeBackend,
    /// Per-segment progress callback. Invoked synchronously after each
    /// segment is fully processed (demuxed +, for streaming paths, written
    /// to disk). `None` (default) skips the callback entirely.
    pub on_progress: Option<Arc<dyn Fn(TransmuxProgress) + Send + Sync>>,
    /// Cooperative cancellation token. Checked at the top of each segment
    /// iteration and raced against `Source::read_bytes` await points.
    pub cancel: Option<Arc<dyn CancelToken>>,
    /// Resume checkpoint. `None` (default) starts a fresh transmux. `Some`
    /// resumes an interrupted run by skipping `segments[..completed_segments]`
    /// and appending to the output file. Only supported for
    /// [`OutputFormat::StreamingMp4`] and [`OutputFormat::FragmentedMp4`].
    pub resume: Option<TransmuxResumeState>,
}

impl std::fmt::Debug for TransmuxOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TransmuxOptions")
            .field("variant", &self.variant)
            .field("output_format", &self.output_format)
            .field("finalize_backend", &self.finalize_backend)
            .field("on_progress", &self.on_progress.as_ref().map(|_| "<callback>"))
            .field("cancel", &self.cancel.as_ref().map(|_| "<cancel token>"))
            .field("resume", &self.resume)
            .finish()
    }
}

/// One progress event, emitted via [`TransmuxOptions::on_progress`] after a
/// segment is fully processed.
///
/// The `resume` field is a fresh checkpoint snapshot; callers should persist
/// it on every callback so a crash or cancel can be resumed from the last
/// fully-written segment.
#[derive(Debug, Clone)]
pub struct TransmuxProgress {
    /// Total segments in the media playlist.
    pub total_segments: usize,
    /// Segments fully processed so far.
    pub completed_segments: usize,
    /// Cumulative downloaded segment bytes (excludes init segments).
    pub downloaded_bytes: u64,
    /// Bytes written to the output file. 0 for the [`Mp4`](OutputFormat::Mp4)
    /// batch path (which buffers in memory until the end).
    pub bytes_written: u64,
    /// Index of the segment just completed.
    pub current_segment_index: usize,
    /// Current resume checkpoint snapshot. Callers should persist this on
    /// every callback so a crash/cancel can be resumed.
    pub resume: TransmuxResumeState,
}

/// Internal bundle of the optional hooks extracted from `TransmuxOptions`,
/// passed down to the per-segment loops. Holds borrowed `Arc`s so the loops
/// can invoke the callback / check cancellation without re-reading the
/// `Option`s each iteration.
struct Hooks<'a> {
    on_progress: Option<&'a Arc<dyn Fn(TransmuxProgress) + Send + Sync>>,
    cancel: Option<&'a Arc<dyn CancelToken>>,
}

impl<'a> Hooks<'a> {
    /// Returns `Err(Error::Cancelled)` if the cancel token has been triggered.
    fn check_cancel(&self) -> Result<()> {
        if let Some(c) = self.cancel {
            if c.is_cancelled() {
                return Err(Error::Cancelled);
            }
        }
        Ok(())
    }

    /// Emits a progress event if a callback is configured. No-op otherwise.
    fn emit(&self, progress: TransmuxProgress) {
        if let Some(cb) = self.on_progress {
            cb(progress);
        }
    }
}

#[derive(Debug, Default)]
struct PacketCollector {
    packets: Vec<EncodedPacket>,
    vps: Option<Vec<u8>>,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
    audio_specific_config: Option<Vec<u8>>,
    sample_rate: Option<u32>,
    channel_count: Option<u8>,
}

/// Remuxes an HLS playlist to an MP4 file.
///
/// Supports local paths and HTTP/HTTPS URLs, master playlists (with an explicit
/// [`VariantSelection`]), `#EXT-X-BYTERANGE` segments, fMP4/CMAF input via
/// `#EXT-X-MAP`, and both non-fragmented and fragmented MP4 output (selected via
/// [`OutputFormat`]). Master playlists require `options.variant` to be set.
pub async fn transmux_hls_to_mp4_async(
    input: HlsInput,
    output: impl AsRef<Path>,
    options: TransmuxOptions,
) -> Result<TransmuxReport> {
    let output = output.as_ref();
    let (root_location, source) = input.into_parts()?;
    let reader = SourceReader::new(source);
    let root_resource = reader.read_text(&root_location).await?;
    let root_playlist = parse_hls_playlist_content(None, &root_resource.content)?;

    let (media_playlist, media_location) = match root_playlist {
        HlsPlaylist::Media(media) => (media, root_resource.location),
        HlsPlaylist::Master(master) => {
            let Some(selection) = options.variant else {
                return Err(Error::invalid(
                    "master playlist input requires TransmuxOptions.variant",
                ));
            };
            let index = selection.select_index(&master)?;
            let variant = &master.variants[index];
            let variant_location = root_resource.location.resolve(&variant.uri)?;
            let variant_resource = reader.read_text(&variant_location).await?;
            let playlist = parse_hls_playlist_content(None, &variant_resource.content)?;
            let HlsPlaylist::Media(media) = playlist else {
                return Err(Error::unsupported(
                    "nested master playlists are out of the Phase 2 slice",
                ));
            };
            (media, variant_resource.location)
        }
    };

    // `Mp4` batch path can't resume (it buffers everything in memory and
    // writes once at the end — there's nothing to append to). Reject early
    // so callers get a clear error instead of silent fallback.
    if options.resume.is_some() && matches!(options.output_format, OutputFormat::Mp4) {
        return Err(Error::invalid(
            "resume is only supported with OutputFormat::StreamingMp4 or FragmentedMp4",
        ));
    }

    let hooks = Hooks {
        on_progress: options.on_progress.as_ref(),
        cancel: options.cancel.as_ref(),
    };

    match options.output_format {
        OutputFormat::Mp4 => {
            mux_to_mp4(&reader, &media_location, &media_playlist, output, &hooks).await
        }
        OutputFormat::FragmentedMp4 => {
            transmux_fragmented_async(
                &reader,
                &media_location,
                &media_playlist,
                output,
                &hooks,
                options.resume.clone(),
            )
            .await
        }
        OutputFormat::StreamingMp4 => {
            // Stage 1: stream the fMP4 pipeline to a temp file (low memory:
            // sample data lands on disk as each segment is demuxed). Stage 2:
            // read it back and defrag into a single ftyp + moov + mdat
            // (faststart). The temp file is a playable fMP4 if interrupted.
            let temp_path = temp_fmp4_path(output);
            let stage1 = transmux_fragmented_async(
                &reader,
                &media_location,
                &media_playlist,
                &temp_path,
                &hooks,
                options.resume.clone(),
            )
            .await;
            if let Err(e) = stage1 {
                // On cancel, keep the .partial.mp4 so the caller can resume.
                // Only clean up on actual errors (non-Cancelled).
                if !matches!(e, Error::Cancelled) {
                    let _ = tokio::fs::remove_file(&temp_path).await;
                }
                return Err(e);
            }
            let segment_count = media_playlist.segments.len();

            // Stage 2: defrag. Native path uses the crate's own ISOBMFF demux
            // + MP4 mux; ffmpeg path (behind `ffmpeg-finalize`) delegates to
            // ffmpeg-next for the remux.
            #[cfg(feature = "ffmpeg-finalize")]
            let result = match options.finalize_backend {
                FinalizeBackend::Native => {
                    defragment_fmp4_to_mp4(&temp_path, output, segment_count).await
                }
                FinalizeBackend::Ffmpeg => {
                    crate::ffmpeg_finalize::remux_to_mp4(&temp_path, output, segment_count).await
                }
            };
            #[cfg(not(feature = "ffmpeg-finalize"))]
            let result = defragment_fmp4_to_mp4(&temp_path, output, segment_count).await;

            let _ = tokio::fs::remove_file(&temp_path).await;
            result
        }
    }
}

/// Streaming demux + standard MP4 mux. Downloads and demuxes one HLS segment
/// at a time into a [`PacketCollector`], then muxes everything into a single
/// `ftyp` + `moov` + `mdat` file.
async fn mux_to_mp4(
    reader: &SourceReader,
    media_location: &SourceLocation,
    media_playlist: &MediaPlaylist,
    output: &Path,
    hooks: &Hooks<'_>,
) -> Result<TransmuxReport> {
    let mut collector = PacketCollector::default();
    read_media_segments(reader, media_location, media_playlist, &mut collector, hooks).await?;

    let (mp4, mut report) = mux_collected_packets(collector, media_playlist.segments.len())?;
    tokio::fs::write(output, &mp4).await?;
    report.bytes_written = mp4.len() as u64;
    Ok(report)
}

/// Stage 2 of the finalized-fragmented path: read a fragmented MP4 produced
/// by stage 1 and defragment it into a standard MP4 (`ftyp` + `moov` +
/// `mdat`, faststart). The fMP4 is parsed with the same ISOBMFF demuxer used
/// for HLS fMP4 inputs — the file is split at the `moov` boundary into an
/// init portion (`ftyp` + `moov`) and a media portion (everything after),
/// then fed to `demux_isobmff` which walks every `moof`/`trun` to recover
/// samples. The samples are then re-muxed via `Mp4Muxer`.
async fn defragment_fmp4_to_mp4(
    fmp4_path: &Path,
    output: &Path,
    segment_count: usize,
) -> Result<TransmuxReport> {
    let data = tokio::fs::read(fmp4_path).await?;
    let init_end = split_fmp4_init(&data)?;

    let demuxed = demux_isobmff(&data[..init_end], &data[init_end..])?;
    let mut collector = PacketCollector::default();
    collector.push_demuxed(demuxed)?;

    let (mp4, mut report) = mux_collected_packets(collector, segment_count)?;
    tokio::fs::write(output, &mp4).await?;
    report.bytes_written = mp4.len() as u64;
    Ok(report)
}

/// Splits a fragmented MP4 file into init (`ftyp` + `moov`) and media (the
/// rest). Returns the byte offset where the media portion begins.
fn split_fmp4_init(data: &[u8]) -> Result<usize> {
    let mut offset = 0;
    let mut saw_ftyp = false;
    while offset + 8 <= data.len() {
        let size = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as u64;
        let btype = &data[offset + 4..offset + 8];
        let total = if size == 1 {
            if offset + 16 > data.len() {
                return Err(Error::bitstream("fMP4 box header truncated"));
            }
            u64::from_be_bytes([
                data[offset + 8],
                data[offset + 9],
                data[offset + 10],
                data[offset + 11],
                data[offset + 12],
                data[offset + 13],
                data[offset + 14],
                data[offset + 15],
            ])
        } else if size == 0 {
            (data.len() - offset) as u64
        } else {
            size
        };
        if btype == b"ftyp" {
            saw_ftyp = true;
        } else if btype == b"moov" {
            if !saw_ftyp {
                return Err(Error::bitstream("fMP4 moov before ftyp"));
            }
            return Ok(offset + total as usize);
        }
        offset += total as usize;
    }
    Err(Error::bitstream("fMP4 missing ftyp or moov"))
}

/// Temp file path for stage 1 of the finalized-fragmented path. Uses `.mp4`
/// (a real fragmented MP4 file) so that if the process is interrupted the
/// temp file is still a playable fMP4.
fn temp_fmp4_path(output: &Path) -> PathBuf {
    let mut p = output.to_path_buf();
    let stem = p.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    let ext = p.extension().map(|s| s.to_os_string()).unwrap_or_default();
    let mut name = stem;
    name.push(".partial");
    if !ext.is_empty() {
        name.push(".");
        name.push(&ext);
    }
    p.set_file_name(name);
    p
}

async fn transmux_fragmented_async(
    reader: &SourceReader,
    media_location: &SourceLocation,
    media_playlist: &MediaPlaylist,
    output: &Path,
    hooks: &Hooks<'_>,
    resume: Option<TransmuxResumeState>,
) -> Result<TransmuxReport> {
    use tokio::io::AsyncWriteExt;

    let segments = &media_playlist.segments;
    if segments.is_empty() {
        return Err(Error::invalid("media playlist contains no segments"));
    }

    // --- Resume checkpoint validation. We need a non-empty playlist with at
    // least one unprocessed segment to continue from. Rejecting here keeps
    // the loop invariant (`start_index < segments.len()`) trivially true.
    if let Some(r) = &resume {
        if r.completed_segments >= segments.len() {
            return Err(Error::invalid(
                "resume state indicates the playlist is already complete",
            ));
        }
    }

    // --- Output file. Fresh run creates + truncates; resume opens in append
    // mode at the existing EOF (the byte offset recorded in the checkpoint).
    // `output_path` is kept for the resume-path file scan (rebuilds tfra).
    let output_path = output;
    let mut output = if resume.is_some() {
        tokio::fs::OpenOptions::new()
            .append(true)
            .open(output_path)
            .await?
    } else {
        tokio::fs::File::create(output_path).await?
    };

    let mut muxer: Option<FragmentedMp4Muxer> = None;
    let mut layout = TrackLayout::default();
    let mut init_cache: Option<(String, Vec<u8>)> = None;
    let mut saved_tracks: Option<Vec<FragmentedTrack>> = None;
    let mut max_duration_ms = 0_u64;
    let mut downloaded_bytes = 0_u64;

    // Per-track tfra entries accumulated as each fragment is streamed to disk;
    // consumed by the trailing mfra box at the end. Fresh runs start empty and
    // are sized inside the loop when the muxer is first created. Resumed runs
    // are pre-populated with historical entries rebuilt from the existing
    // `.partial.mp4` (plan §5.5 enhancement), then new entries are appended as
    // fresh fragments are written — producing a complete mfra at EOF.
    let mut tfra_entries_per_track: Vec<Vec<TfraEntry>> = Vec::new();

    // --- Resume: restore the four checkpoint fields. `tracks` is re-extracted
    // by re-demuxing segments[0] (codec config must match the original; the
    // checkpoint does not persist SPS/PPS/ASC bytes — see plan §5.3).
    // Historical tfra entries are rebuilt by scanning the existing
    // `.partial.mp4`'s moof boxes (plan §5.5 enhancement), so resumed runs
    // now emit a complete mfra box matching a fresh run's output.
    let mut bytes_written: u64 = resume.as_ref().map(|r| r.bytes_written).unwrap_or(0);
    let mut global_base_dts_90k: Option<u64> =
        resume.as_ref().map(|r| r.global_base_dts_90k);

    if let Some(r) = &resume {
        // Re-demux segments[0] to rebuild codec config (tracks). The bytes
        // are downloaded but not written to the output — only the demux
        // metadata (SPS/PPS/ASC) is used to construct the muxer.
        let first_segment = &segments[0];
        let (first_demuxed, _first_bytes) =
            demux_segment(reader, media_location, first_segment, &mut init_cache).await?;
        let tracks = build_fragmented_tracks(&first_demuxed)?;
        layout = TrackLayout::from_tracks(&tracks);
        let m = FragmentedMp4Muxer::new_with_sequence(tracks.clone(), r.next_sequence);
        saved_tracks = Some(tracks);
        muxer = Some(m);

        // Rebuild historical tfra entries by scanning the existing
        // `.partial.mp4` (plan §5.5 enhancement). The file's moof boxes are
        // walked to recover each fragment's track ID, base decode time, and
        // absolute moof offset. These are mapped to track indices via the
        // rebuilt tracks and pushed into tfra_entries_per_track, so the
        // resumed run emits a complete mfra box at EOF (matching a fresh
        // run's output byte-for-byte).
        let existing = tokio::fs::read(output_path).await?;
        // Scan only up to the checkpoint's bytes_written — the file may have
        // been opened in append mode but nothing has been written yet by this
        // run, so in practice file size == bytes_written. The min() guards
        // against accidental truncation.
        let scan_end = (r.bytes_written as usize).min(existing.len());
        let scanned = crate::isobmff::extract_tfra_entries(&existing[..scan_end])?;
        let tracks_ref = saved_tracks.as_ref().expect("tracks were just saved");
        let track_id_to_index: std::collections::HashMap<u32, usize> = tracks_ref
            .iter()
            .enumerate()
            .map(|(i, t)| (t.track_id, i))
            .collect();
        tfra_entries_per_track = (0..tracks_ref.len()).map(|_| Vec::new()).collect();
        for entry in scanned {
            if let Some(&track_index) = track_id_to_index.get(&entry.track_id) {
                tfra_entries_per_track[track_index].push(TfraEntry {
                    time: entry.base_decode_time,
                    moof_offset: entry.moof_offset,
                    traf_number: 1,
                    trun_number: 1,
                    sample_number: 1,
                });
            }
        }
    }

    let start_index = resume.as_ref().map(|r| r.completed_segments).unwrap_or(0);

    // --- Streaming: write header (fresh run only), then one (styp + moof +
    // mdat) per segment directly to the file. The temp file grows as segments
    // are demuxed, so an interrupted run still leaves a playable fMP4 (ftyp +
    // moov + the fragments written so far). sidx is intentionally omitted: it
    // must reference the total size of all fragments, which is unknown until
    // the end, and writing it would require buffering everything in memory
    // (the exact pattern we are avoiding). mfra is appended at EOF instead.
    for (loop_index, segment) in segments[start_index..].iter().enumerate() {
        // Cooperative cancellation: check at the top of each iteration so a
        // cancelled run stops before downloading the next segment.
        hooks.check_cancel()?;

        let segment_index = start_index + loop_index;
        let (demuxed, segment_bytes) =
            demux_segment(reader, media_location, segment, &mut init_cache).await?;
        downloaded_bytes += segment_bytes;

        // Capture the global base DTS from the first packet we ever see, so
        // every sample across all segments is shifted to a zero-based timeline.
        // Skipped on resume: the checkpoint already carries the original base.
        if global_base_dts_90k.is_none() {
            if let Some(first) = demuxed.packets.first() {
                global_base_dts_90k = Some(first.dts_90k);
            }
        }
        let base = global_base_dts_90k.unwrap_or(0);

        if muxer.is_none() {
            let tracks = build_fragmented_tracks(&demuxed)?;
            layout = TrackLayout::from_tracks(&tracks);
            tfra_entries_per_track = (0..tracks.len()).map(|_| Vec::new()).collect();
            let m = FragmentedMp4Muxer::new(tracks.clone());
            let header = m.write_header()?;
            output.write_all(&header).await?;
            bytes_written += header.len() as u64;
            saved_tracks = Some(tracks);
            muxer = Some(m);
        }

        let muxer = muxer.as_mut().expect("muxer was just initialized");
        let samples_per_track = group_samples_per_track(&demuxed.packets, &layout, base)?;

        // Record per-track tfra entries before writing the fragment, using the
        // current file offset (pointing at the styp that starts this fragment).
        // Each fragment begins with an styp box; its size (first 4 bytes) is
        // the offset of the moof within the fragment.
        let pre_write_offset = bytes_written;
        for (track_index, samples) in samples_per_track.iter().enumerate() {
            if let Some(first) = samples.first() {
                // moof_offset is filled after we know the styp size below;
                // for now record the fragment start and update afterwards.
                // We'll fix this by computing styp size from the fragment bytes.
                tfra_entries_per_track[track_index].push(TfraEntry {
                    time: first.dts,
                    moof_offset: 0, // placeholder, fixed after write_fragment
                    traf_number: 1,
                    trun_number: 1,
                    sample_number: 1,
                });
            }
        }

        let fragment_bytes = muxer.write_fragment(&samples_per_track)?;
        // Each fragment starts with an styp box; its size field (first 4
        // bytes, big-endian) gives us the moof offset relative to the
        // fragment start.
        let styp_size = u32::from_be_bytes(fragment_bytes[0..4].try_into().unwrap()) as u64;

        // Fix up the moof_offset for the entries we just pushed.
        for track_index in 0..samples_per_track.len() {
            if !samples_per_track[track_index].is_empty() {
                let entry = tfra_entries_per_track[track_index]
                    .last_mut()
                    .expect("entry was just pushed");
                entry.moof_offset = pre_write_offset + styp_size;
            }
        }

        output.write_all(&fragment_bytes).await?;
        bytes_written += fragment_bytes.len() as u64;

        if let Some(last) = demuxed.packets.last() {
            let ms = last.dts_90k.saturating_sub(base).saturating_mul(1000) / 90_000;
            if ms > max_duration_ms {
                max_duration_ms = ms;
            }
        }

        // Emit progress with a fresh resume snapshot. The caller should
        // persist this on every callback so a crash/cancel can resume from
        // the last fully-written fragment.
        let next_sequence = muxer.next_sequence();
        let progress_base = global_base_dts_90k.unwrap_or(0);
        hooks.emit(TransmuxProgress {
            total_segments: segments.len(),
            completed_segments: segment_index + 1,
            downloaded_bytes,
            bytes_written,
            current_segment_index: segment_index,
            resume: TransmuxResumeState {
                completed_segments: segment_index + 1,
                bytes_written,
                next_sequence,
                global_base_dts_90k: progress_base,
            },
        });
    }

    if saved_tracks.is_none() {
        return Err(Error::invalid("no segments were processed"));
    }

    // mfra at EOF lets players find sync samples by seeking from the end.
    // Resumed runs rebuild historical tfra entries by scanning the existing
    // `.partial.mp4` (plan §5.5), so both fresh and resumed runs emit a
    // complete mfra box — the outputs are byte-identical (after wall-clock
    // timestamp normalization).
    if !tfra_entries_per_track.is_empty() {
        let tracks_ref = saved_tracks.as_ref().expect("tracks were saved");
        let mfra = mfra_box(tracks_ref, &tfra_entries_per_track)?;
        output.write_all(&mfra).await?;
        bytes_written += mfra.len() as u64;
    }

    output.flush().await?;

    Ok(TransmuxReport {
        segment_count: media_playlist.segments.len(),
        tracks: Vec::new(),
        duration: max_duration_ms,
        duration_timescale: 1000,
        bytes_written,
    })
}

#[derive(Debug, Clone, Copy, Default)]
struct TrackLayout {
    /// Index of the video track in the muxer, if present.
    video_index: Option<usize>,
    /// Index of the audio track in the muxer, if present.
    audio_index: Option<usize>,
    /// Audio timescale (AAC sample rate) for rescaling 90 kHz timestamps.
    audio_timescale: u32,
}

impl TrackLayout {
    fn from_tracks(tracks: &[FragmentedTrack]) -> Self {
        use crate::mp4::FragmentedTrackKind;
        let mut layout = Self::default();
        for (index, track) in tracks.iter().enumerate() {
            match &track.kind {
                FragmentedTrackKind::Video { .. } => layout.video_index = Some(index),
                FragmentedTrackKind::Audio { sample_rate, .. } => {
                    layout.audio_index = Some(index);
                    layout.audio_timescale = *sample_rate;
                }
            }
        }
        layout
    }

    fn track_count(&self) -> usize {
        self.video_index.is_some() as usize + self.audio_index.is_some() as usize
    }
}

/// Demux a single HLS segment, handling both TS and fMP4/CMAF inputs.
///
/// Returns the demuxed packets plus the segment's raw byte length (excluding
/// any init segment). The byte length is used to populate progress callbacks.
async fn demux_segment(
    reader: &SourceReader,
    playlist_location: &SourceLocation,
    segment: &crate::hls::HlsSegment,
    init_cache: &mut Option<(String, Vec<u8>)>,
) -> Result<(DemuxOutput, u64)> {
    let location = playlist_location.resolve(&segment.uri)?;
    let data = reader
        .read_bytes(&location, segment.byte_range.as_ref())
        .await?;
    let segment_bytes = data.len() as u64;

    let demuxed = if let Some(init_spec) = &segment.init_segment {
        let init_bytes = if let Some((_, cached_bytes)) = init_cache
            .as_ref()
            .filter(|(uri, _)| uri == &init_spec.uri)
        {
            cached_bytes.clone()
        } else {
            let init_location = playlist_location.resolve(&init_spec.uri)?;
            let bytes = reader
                .read_bytes(&init_location, init_spec.byte_range.as_ref())
                .await?;
            *init_cache = Some((init_spec.uri.clone(), bytes.clone()));
            bytes
        };
        demux_isobmff(&init_bytes, &data)?
    } else {
        demux_ts(&data)?
    };
    Ok((demuxed, segment_bytes))
}

fn build_fragmented_tracks(first: &DemuxOutput) -> Result<Vec<FragmentedTrack>> {
    let mut tracks = Vec::new();
    let mut next_track_id = 1_u32;

    if first.saw_video {
        if let Some(vps) = &first.vps {
            let sps = first
                .sps
                .as_ref()
                .ok_or_else(|| Error::bitstream("HEVC SPS was not found in first segment"))?;
            let pps = first
                .pps
                .as_ref()
                .ok_or_else(|| Error::bitstream("HEVC PPS was not found in first segment"))?;
            tracks.push(FragmentedTrack::hevc_video(next_track_id, vps, sps, pps)?);
        } else {
            let sps = first
                .sps
                .as_ref()
                .ok_or_else(|| Error::bitstream("H.264 SPS was not found in first segment"))?;
            let pps = first
                .pps
                .as_ref()
                .ok_or_else(|| Error::bitstream("H.264 PPS was not found in first segment"))?;
            tracks.push(FragmentedTrack::avc_video(next_track_id, sps, pps)?);
        }
        next_track_id += 1;
    }

    if first.saw_audio {
        let sample_rate = first
            .sample_rate
            .ok_or_else(|| Error::bitstream("AAC sample rate was not found in first segment"))?;
        let channel_count = first
            .channel_count
            .ok_or_else(|| Error::bitstream("AAC channel count was not found in first segment"))?;
        let asc = first.audio_specific_config.clone().ok_or_else(|| {
            Error::bitstream("AAC AudioSpecificConfig was not found in first segment")
        })?;
        tracks.push(FragmentedTrack::audio(
            next_track_id,
            sample_rate,
            channel_count,
            asc,
        ));
    }

    if tracks.is_empty() {
        return Err(Error::invalid("first segment produced no tracks"));
    }
    Ok(tracks)
}

/// Groups a segment's packets into per-track `Mp4Sample` vectors matching the
/// muxer's track layout. Tracks with no samples in this segment get an empty vec.
///
/// `base_dts_90k` is the global base DTS (90k domain) captured from the first
/// packet of the first segment. All sample DTS/PTS are shifted by this base so
/// the timeline starts at 0; this keeps tfdt values correct (fragment 0 starts
/// at 0, later fragments at their cumulative offset) without inflating the
/// track duration with a non-zero TS encoder initial timestamp.
fn group_samples_per_track(
    packets: &[EncodedPacket],
    layout: &TrackLayout,
    base_dts_90k: u64,
) -> Result<Vec<Vec<Mp4Sample>>> {
    let mut video_samples: Vec<Mp4Sample> = Vec::new();
    let mut audio_samples: Vec<Mp4Sample> = Vec::new();

    for packet in packets {
        match packet.kind {
            StreamKind::Avc | StreamKind::Hevc => {
                let data = if packet.is_length_prefixed {
                    packet.data.clone()
                } else {
                    avc::annex_b_to_length_prefixed(&packet.data)?
                };
                // Shift to a zero-based timeline using the global base so tfdt
                // starts at 0 and the track duration is not inflated by the
                // TS encoder's initial timestamp offset.
                video_samples.push(Mp4Sample {
                    data,
                    dts: packet.dts_90k.saturating_sub(base_dts_90k),
                    pts: packet.pts_90k.saturating_sub(base_dts_90k),
                    duration: 0,
                    is_key: packet.is_key,
                    offset: 0,
                });
            }
            StreamKind::Aac => {
                let audio_ts = if layout.audio_timescale > 0 {
                    layout.audio_timescale
                } else {
                    90_000
                };
                let dts = rescale_90k(packet.dts_90k.saturating_sub(base_dts_90k), audio_ts);
                let pts = rescale_90k(packet.pts_90k.saturating_sub(base_dts_90k), audio_ts);
                audio_samples.push(Mp4Sample {
                    data: packet.data.clone(),
                    dts,
                    pts,
                    duration: 1024,
                    is_key: true,
                    offset: 0,
                });
            }
        }
    }

    // Build the output in track-index order, with empty vecs for missing tracks.
    let track_count = layout.track_count();
    let mut out: Vec<Vec<Mp4Sample>> = (0..track_count).map(|_| Vec::new()).collect();
    if let Some(vi) = layout.video_index {
        if !video_samples.is_empty() {
            assign_delta_durations(&mut video_samples)?;
        }
        out[vi] = video_samples;
    }
    if let Some(ai) = layout.audio_index {
        out[ai] = audio_samples;
    }
    Ok(out)
}

async fn read_media_segments(
    reader: &SourceReader,
    playlist_location: &SourceLocation,
    playlist: &MediaPlaylist,
    collector: &mut PacketCollector,
    hooks: &Hooks<'_>,
) -> Result<()> {
    let mut init_cache: Option<(String, Vec<u8>)> = None;
    let mut downloaded_bytes = 0_u64;
    let total_segments = playlist.segments.len();

    for (index, segment) in playlist.segments.iter().enumerate() {
        // Cooperative cancellation: check before downloading each segment.
        hooks.check_cancel()?;

        let (demuxed, segment_bytes) =
            demux_segment(reader, playlist_location, segment, &mut init_cache).await?;
        downloaded_bytes += segment_bytes;
        collector.push_demuxed(demuxed)?;

        // Mp4 batch path doesn't write to disk per-segment, so bytes_written
        // stays 0 and the resume snapshot is informational only — Mp4 output
        // does not support resume (rejected at the entry point).
        hooks.emit(TransmuxProgress {
            total_segments,
            completed_segments: index + 1,
            downloaded_bytes,
            bytes_written: 0,
            current_segment_index: index,
            resume: TransmuxResumeState {
                completed_segments: index + 1,
                bytes_written: 0,
                next_sequence: (index + 1) as u32,
                global_base_dts_90k: 0,
            },
        });
    }
    Ok(())
}

impl PacketCollector {
    fn push_demuxed(&mut self, demuxed: DemuxOutput) -> Result<()> {
        if let Some(segment_vps) = demuxed.vps {
            update_param(
                &mut self.vps,
                segment_vps,
                "mid-stream HEVC VPS changes are out of Phase 3 scope",
            )?;
        }
        if let Some(segment_sps) = demuxed.sps {
            update_param(
                &mut self.sps,
                segment_sps,
                "mid-stream SPS changes are out of Phase 3 scope",
            )?;
        }
        if let Some(segment_pps) = demuxed.pps {
            update_param(
                &mut self.pps,
                segment_pps,
                "mid-stream PPS changes are out of Phase 3 scope",
            )?;
        }
        if let Some(config) = demuxed.audio_specific_config {
            update_param(
                &mut self.audio_specific_config,
                config,
                "mid-stream AAC config changes are out of Phase 3 scope",
            )?;
        }
        if let Some(rate) = demuxed.sample_rate {
            if self.sample_rate.is_some_and(|existing| existing != rate) {
                return Err(Error::unsupported(
                    "mid-stream AAC sample rate changes are out of Phase 3 scope",
                ));
            }
            self.sample_rate = Some(rate);
        }
        if let Some(channels) = demuxed.channel_count {
            if self
                .channel_count
                .is_some_and(|existing| existing != channels)
            {
                return Err(Error::unsupported(
                    "mid-stream AAC channel count changes are out of Phase 3 scope",
                ));
            }
            self.channel_count = Some(channels);
        }

        self.packets.extend(demuxed.packets);
        Ok(())
    }
}

fn update_param(
    slot: &mut Option<Vec<u8>>,
    new_value: Vec<u8>,
    conflict_msg: &str,
) -> Result<()> {
    if let Some(existing) = slot {
        if existing != &new_value {
            return Err(Error::unsupported(conflict_msg));
        }
    } else {
        *slot = Some(new_value);
    }
    Ok(())
}

fn mux_collected_packets(
    collector: PacketCollector,
    segment_count: usize,
) -> Result<(Vec<u8>, TransmuxReport)> {
    let PacketCollector {
        packets,
        vps,
        sps,
        pps,
        audio_specific_config,
        sample_rate,
        channel_count,
    } = collector;

    if packets.is_empty() {
        return Err(Error::invalid(
            "HLS playlist did not produce any encoded packets",
        ));
    }

    let base_dts = packets
        .iter()
        .map(|packet| packet.dts_90k)
        .min()
        .ok_or_else(|| Error::invalid("HLS playlist did not produce any encoded packets"))?;
    let sample_rate =
        sample_rate.ok_or_else(|| Error::unsupported("AAC audio is required for non-fragmented MP4"))?;
    let channel_count = channel_count
        .ok_or_else(|| Error::unsupported("AAC audio is required for non-fragmented MP4"))?;

    let mut video_samples = Vec::new();
    let mut audio_samples = Vec::new();
    for packet in packets {
        if packet.dts_90k < base_dts || packet.pts_90k < base_dts {
            return Err(Error::muxing(
                "packet timestamps precede the normalization base",
            ));
        }

        match packet.kind {
            StreamKind::Avc | StreamKind::Hevc => {
                let data = if packet.is_length_prefixed {
                    packet.data
                } else {
                    // Annex B — reuse the AVC start-code scanner for both codecs
                    // (HEVC and AVC share the 0x000001 / 0x00000001 start code syntax).
                    avc::annex_b_to_length_prefixed(&packet.data)?
                };
                video_samples.push(Mp4Sample {
                    data,
                    dts: packet.dts_90k - base_dts,
                    pts: packet.pts_90k - base_dts,
                    duration: 0,
                    is_key: packet.is_key,
                    offset: 0,
                });
            }
            StreamKind::Aac => {
                let dts = rescale_90k(packet.dts_90k - base_dts, sample_rate);
                let pts = rescale_90k(packet.pts_90k - base_dts, sample_rate);
                audio_samples.push(Mp4Sample {
                    data: packet.data,
                    dts,
                    pts,
                    duration: u32::try_from(packet.duration)
                        .map_err(|_| Error::muxing("AAC packet duration exceeds u32"))?,
                    is_key: true,
                    offset: 0,
                });
            }
        }
    }

    let audio_specific_config = audio_specific_config
        .ok_or_else(|| Error::bitstream("AAC AudioSpecificConfig was not found"))?;

    let video_track = if let Some(vps) = vps {
        let sps = sps.ok_or_else(|| Error::bitstream("HEVC SPS was not found"))?;
        let pps = pps.ok_or_else(|| Error::bitstream("HEVC PPS was not found"))?;
        make_hevc_video_track(video_samples, &vps, &sps, &pps)?
    } else {
        let sps = sps.ok_or_else(|| Error::bitstream("H.264 SPS was not found"))?;
        let pps = pps.ok_or_else(|| Error::bitstream("H.264 PPS was not found"))?;
        make_video_track(video_samples, &sps, &pps)?
    };

    let tracks = vec![
        video_track,
        make_audio_track(
            audio_samples,
            sample_rate,
            channel_count,
            audio_specific_config,
        )?,
    ];
    let (mp4, track_infos) = Mp4Muxer::new(tracks).write()?;

    let duration = track_infos
        .iter()
        .map(|track| track.duration.saturating_mul(1000) / u64::from(track.timescale))
        .max()
        .unwrap_or(0);

    Ok((
        mp4,
        TransmuxReport {
            segment_count,
            tracks: track_infos,
            duration,
            duration_timescale: 1000,
            bytes_written: 0,
        },
    ))
}

fn rescale_90k(value: u64, to_timescale: u32) -> u64 {
    value.saturating_mul(u64::from(to_timescale)) / 90_000
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "default-source")]
    use std::fs;

    use super::*;
    use crate::hls::VariantStream;

    fn variant(uri: &str, bandwidth: Option<u64>) -> VariantStream {
        VariantStream {
            uri: uri.to_string(),
            path: PathBuf::new(),
            bandwidth,
            resolution: None,
            codecs: None,
        }
    }

    fn master(variants: Vec<VariantStream>) -> MasterPlaylist {
        MasterPlaylist {
            path: PathBuf::new(),
            variants,
        }
    }

    #[test]
    fn rescales_90k_to_audio_timescale() {
        assert_eq!(rescale_90k(90_000, 44_100), 44_100);
    }

    #[test]
    fn variant_selection_index_returns_specified() {
        let m = master(vec![variant("a.m3u8", Some(100)), variant("b.m3u8", Some(200))]);
        assert_eq!(VariantSelection::Index(0).select_index(&m).unwrap(), 0);
        assert_eq!(VariantSelection::Index(1).select_index(&m).unwrap(), 1);
    }

    #[test]
    fn variant_selection_index_out_of_range_errors() {
        let m = master(vec![variant("a.m3u8", Some(100))]);
        let err = VariantSelection::Index(5).select_index(&m).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn variant_selection_highest_bandwidth_picks_max() {
        let m = master(vec![
            variant("low.m3u8", Some(500_000)),
            variant("mid.m3u8", Some(1_500_000)),
            variant("high.m3u8", Some(3_000_000)),
        ]);
        assert_eq!(
            VariantSelection::HighestBandwidth.select_index(&m).unwrap(),
            2
        );
    }

    #[test]
    fn variant_selection_lowest_bandwidth_picks_min() {
        let m = master(vec![
            variant("low.m3u8", Some(500_000)),
            variant("mid.m3u8", Some(1_500_000)),
            variant("high.m3u8", Some(3_000_000)),
        ]);
        assert_eq!(
            VariantSelection::LowestBandwidth.select_index(&m).unwrap(),
            0
        );
    }

    #[test]
    fn variant_selection_highest_bandwidth_with_missing_bandwidth_picks_real() {
        // Variants without BANDWIDTH are treated as 0 for HighestBandwidth,
        // so a real-bandwidth variant always wins. When ALL lack bandwidth,
        // max_by_key returns the last (Rust tie-breaking).
        let all_missing = master(vec![variant("a.m3u8", None), variant("b.m3u8", None)]);
        assert_eq!(
            VariantSelection::HighestBandwidth
                .select_index(&all_missing)
                .unwrap(),
            1
        );

        let mixed = master(vec![
            variant("a.m3u8", None),
            variant("b.m3u8", Some(1_000_000)),
            variant("c.m3u8", None),
        ]);
        assert_eq!(
            VariantSelection::HighestBandwidth.select_index(&mixed).unwrap(),
            1
        );
    }

    #[tokio::test]
    #[cfg(feature = "default-source")]
    async fn rejects_master_without_variant() {
        let temp_dir =
            std::env::temp_dir().join(format!("hls-transmux-master-test-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();
        let master = temp_dir.join("master.m3u8");
        fs::write(
            &master,
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1\nmedia.m3u8\n",
        )
        .unwrap();

        let err = transmux_hls_to_mp4_async(
            HlsInput::Path(master),
            temp_dir.join("out.mp4"),
            TransmuxOptions::default(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }
}
