use std::path::{Path, PathBuf};

use crate::codecs::avc;
use crate::error::{Error, Result};
use crate::hls::{HlsPlaylist, MediaPlaylist, parse_hls_playlist_content};
use crate::isobmff::demux_isobmff;
use crate::mp4::{
    FragmentedMp4Muxer, FragmentedTrack, Mp4Muxer, Mp4Sample, TfraEntry, assign_delta_durations,
    make_audio_track, make_hevc_video_track, make_video_track, mfra_box,
};
use crate::mpeg_ts::demux_ts;
use crate::source::{HlsInput, SourceLocation, SourceReader};
use crate::types::{DemuxOutput, EncodedPacket, StreamKind, TransmuxReport};

/// Selects which variant of a master playlist to transmux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VariantSelection {
    /// Zero-based index into the master playlist's variant list.
    Index(usize),
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TransmuxOptions {
    /// Required when the input is a master playlist; ignored for media playlists.
    pub variant: Option<VariantSelection>,
    /// Output container format / pipeline. See [`OutputFormat`].
    pub output_format: OutputFormat,
    /// Finalization backend for [`OutputFormat::StreamingMp4`]. Ignored for
    /// other formats. Default: [`FinalizeBackend::Native`].
    pub finalize_backend: FinalizeBackend,
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
            let Some(VariantSelection::Index(index)) = options.variant else {
                return Err(Error::invalid(
                    "master playlist input requires TransmuxOptions.variant",
                ));
            };
            let variant = master.variants.get(index).ok_or_else(|| {
                Error::invalid(format!(
                    "variant index {index} is out of range for {} variants",
                    master.variants.len()
                ))
            })?;
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

    match options.output_format {
        OutputFormat::Mp4 => mux_to_mp4(&reader, &media_location, &media_playlist, output).await,
        OutputFormat::FragmentedMp4 => {
            transmux_fragmented_async(&reader, &media_location, &media_playlist, output).await
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
            )
            .await;
            if let Err(e) = stage1 {
                let _ = tokio::fs::remove_file(&temp_path).await;
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
) -> Result<TransmuxReport> {
    let mut collector = PacketCollector::default();
    read_media_segments(reader, media_location, media_playlist, &mut collector).await?;

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
) -> Result<TransmuxReport> {
    use tokio::io::AsyncWriteExt;

    let mut output = tokio::fs::File::create(output).await?;
    let mut muxer: Option<FragmentedMp4Muxer> = None;
    let mut layout = TrackLayout::default();
    let mut init_cache: Option<(String, Vec<u8>)> = None;
    let mut saved_tracks: Option<Vec<FragmentedTrack>> = None;
    let mut max_duration_ms = 0_u64;
    let mut bytes_written = 0_u64;

    // Global base DTS (90k domain) captured from the very first packet across
    // all segments. All sample DTS/PTS are shifted by this so the first
    // fragment's tfdt starts at 0 and subsequent fragments' tfdt reflects the
    // true cumulative position on the (normalized) track timeline. Without
    // this, the first packet's raw 90k DTS (often non-zero due to TS encoder
    // initial timestamp offset) leaks into tfdt, inflating the track duration.
    let mut global_base_dts_90k: Option<u64> = None;

    // Per-track tfra entries accumulated as each fragment is streamed to disk;
    // consumed by the trailing mfra box at the end.
    let mut tfra_entries_per_track: Vec<Vec<TfraEntry>> = Vec::new();

    // --- Streaming: write header, then one (styp + moof + mdat) per segment
    // directly to the file. The temp file grows as segments are demuxed, so
    // an interrupted run still leaves a playable fMP4 (ftyp + moov + the
    // fragments written so far). sidx is intentionally omitted: it must
    // reference the total size of all fragments, which is unknown until the
    // end, and writing it would require buffering everything in memory (the
    // exact pattern we are avoiding). mfra is appended at EOF instead.
    for segment in &media_playlist.segments {
        let demuxed = demux_segment(reader, media_location, segment, &mut init_cache).await?;

        // Capture the global base DTS from the first packet we ever see, so
        // every sample across all segments is shifted to a zero-based timeline.
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
    }

    let tracks = saved_tracks.ok_or_else(|| Error::invalid("no segments were processed"))?;
    if tfra_entries_per_track.is_empty() {
        return Err(Error::invalid("no fragments were generated"));
    }

    // mfra at EOF lets players find sync samples by seeking from the end.
    let mfra = mfra_box(&tracks, &tfra_entries_per_track)?;
    output.write_all(&mfra).await?;
    bytes_written += mfra.len() as u64;

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
async fn demux_segment(
    reader: &SourceReader,
    playlist_location: &SourceLocation,
    segment: &crate::hls::HlsSegment,
    init_cache: &mut Option<(String, Vec<u8>)>,
) -> Result<DemuxOutput> {
    let location = playlist_location.resolve(&segment.uri)?;
    let data = reader
        .read_bytes(&location, segment.byte_range.as_ref())
        .await?;

    if let Some(init_spec) = &segment.init_segment {
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
        demux_isobmff(&init_bytes, &data)
    } else {
        demux_ts(&data)
    }
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
) -> Result<()> {
    let mut init_cache: Option<(String, Vec<u8>)> = None;
    for segment in &playlist.segments {
        let demuxed = demux_segment(reader, playlist_location, segment, &mut init_cache).await?;
        collector.push_demuxed(demuxed)?;
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
    use std::fs;

    use super::*;

    #[test]
    fn rescales_90k_to_audio_timescale() {
        assert_eq!(rescale_90k(90_000, 44_100), 44_100);
    }

    #[tokio::test]
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
