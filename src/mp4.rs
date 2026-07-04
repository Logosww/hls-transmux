use crate::codecs::avc;
use crate::error::{Error, Result};
use crate::types::{Codec, TrackInfo, TrackType};

/// Movie (mvhd) and tkhd timescale. 57600 is the LCM of common frame rates
/// (24/25/30/60/120/240 fps), so rescaling track durations into it introduces
/// no rounding for typical content.
const MOVIE_TIMESCALE: u32 = 57_600;

#[derive(Debug, Clone)]
pub(crate) struct Mp4Sample {
    pub data: Vec<u8>,
    pub dts: u64,
    pub pts: u64,
    pub duration: u32,
    pub is_key: bool,
    pub(crate) offset: u64,
}

#[derive(Debug, Clone)]
pub(crate) enum VideoCodec {
    Avc { avcc: Vec<u8> },
    Hevc { hvcc: Vec<u8> },
}

impl VideoCodec {
    pub(crate) fn codec(&self) -> Codec {
        match self {
            Self::Avc { .. } => Codec::Avc,
            Self::Hevc { .. } => Codec::Hevc,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) enum Mp4Track {
    Video {
        samples: Vec<Mp4Sample>,
        timescale: u32,
        width: u16,
        height: u16,
        codec: VideoCodec,
    },
    Audio {
        samples: Vec<Mp4Sample>,
        timescale: u32,
        sample_rate: u32,
        channel_count: u8,
        audio_specific_config: Vec<u8>,
    },
}

impl Mp4Track {
    pub(crate) fn samples(&self) -> &[Mp4Sample] {
        match self {
            Self::Video { samples, .. } | Self::Audio { samples, .. } => samples,
        }
    }

    fn samples_mut(&mut self) -> &mut [Mp4Sample] {
        match self {
            Self::Video { samples, .. } | Self::Audio { samples, .. } => samples,
        }
    }

    fn track_info(&self) -> TrackInfo {
        match self {
            Self::Video {
                samples,
                timescale,
                width,
                height,
                codec,
                ..
            } => TrackInfo {
                track_type: TrackType::Video,
                codec: codec.codec(),
                timescale: *timescale,
                duration: track_duration(samples),
                sample_count: samples.len(),
                width: Some(*width),
                height: Some(*height),
                sample_rate: None,
                channel_count: None,
            },
            Self::Audio {
                samples,
                timescale,
                sample_rate,
                channel_count,
                ..
            } => TrackInfo {
                track_type: TrackType::Audio,
                codec: Codec::Aac,
                timescale: *timescale,
                duration: track_duration(samples),
                sample_count: samples.len(),
                width: None,
                height: None,
                sample_rate: Some(*sample_rate),
                channel_count: Some(*channel_count),
            },
        }
    }
}

pub(crate) struct Mp4Muxer {
    tracks: Vec<Mp4Track>,
}

impl Mp4Muxer {
    pub(crate) fn new(tracks: Vec<Mp4Track>) -> Self {
        Self { tracks }
    }

    pub(crate) fn write(mut self) -> Result<(Vec<u8>, Vec<TrackInfo>)> {
        if self.tracks.is_empty() {
            return Err(Error::muxing("MP4 output requires at least one track"));
        }

        let ftyp = ftyp_box(&self.tracks);

        // --- Per-track chunking: group each track's samples into ~0.5s chunks.
        // mdat layout is chunk-major (one chunk's samples are contiguous), so
        // stsc/stco can use one entry per chunk instead of one per sample.
        let chunks_per_track: Vec<Vec<ChunkMeta>> = self
            .tracks
            .iter()
            .map(split_chunks)
            .collect();

        // --- Order chunks across tracks by (first_dts, track_index) so the
        // mdat interleaves tracks in presentation order.
        let mut placed: Vec<(u64, usize, usize)> = Vec::new(); // (first_dts, track_idx, chunk_idx_in_track)
        for (track_index, chunks) in chunks_per_track.iter().enumerate() {
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let first_dts = self.tracks[track_index].samples()[chunk.first_sample].dts;
                placed.push((first_dts, track_index, chunk_index));
            }
        }
        placed.sort_by_key(|&(dts, track_index, chunk_index)| (dts, track_index, chunk_index));

        // --- Build mdat payload and record each sample's file offset.
        // We don't know moov size yet, so first pass uses offset = 0 to
        // measure moov, then we patch real offsets in.
        let mdat_payload_size =
            self.tracks
                .iter()
                .flat_map(Mp4Track::samples)
                .try_fold(0_u64, |acc, sample| {
                    acc.checked_add(sample.data.len() as u64)
                        .ok_or_else(|| Error::muxing("MP4 media data is too large"))
                })?;
        if mdat_payload_size + 8 >= u32::MAX as u64 {
            return Err(Error::unsupported(
                "outputs larger than 4 GiB are out of Phase 1 scope",
            ));
        }

        let mut mdat_payload = Vec::with_capacity(mdat_payload_size as usize);
        // mdat_offset of each chunk (relative to mdat payload start).
        let mut chunk_mdat_offsets: Vec<Vec<u64>> =
            chunks_per_track.iter().map(|c| vec![0u64; c.len()]).collect();
        let mut current_mdat_offset: u64 = 0;
        for &(_, track_index, chunk_index) in &placed {
            let chunk = chunks_per_track[track_index][chunk_index];
            chunk_mdat_offsets[track_index][chunk_index] = current_mdat_offset;
            let samples = &self.tracks[track_index].samples()
                [chunk.first_sample..chunk.first_sample + chunk.sample_count as usize];
            for sample in samples {
                mdat_payload.extend_from_slice(&sample.data);
                current_mdat_offset += sample.data.len() as u64;
            }
        }
        debug_assert_eq!(current_mdat_offset, mdat_payload_size);

        // --- First moov pass with dummy offsets to measure its size.
        // stco offsets don't affect moov size (fixed u32 per chunk), so this
        // measurement is exact.
        for track_index in 0..self.tracks.len() {
            for sample in self.tracks[track_index].samples_mut() {
                sample.offset = 0;
            }
        }
        let dummy_moov = moov_box(&self.tracks, &chunks_per_track)?;
        let moov_size = dummy_moov.len();

        // --- Compute real file offsets. faststart layout: ftyp + moov + mdat.
        let mdat_header_size = 8u64;
        let mdat_file_offset = ftyp.len() as u64 + moov_size as u64 + mdat_header_size;
        for track_index in 0..self.tracks.len() {
            let chunks = &chunks_per_track[track_index];
            for (chunk_index, chunk) in chunks.iter().enumerate() {
                let file_offset = mdat_file_offset + chunk_mdat_offsets[track_index][chunk_index];
                // Set the chunk's first sample offset; subsequent samples in
                // the chunk get sequential offsets derived from sample sizes.
                let mut offset = file_offset;
                for sample_index in chunk.first_sample
                    ..chunk.first_sample + chunk.sample_count as usize
                {
                    self.tracks[track_index].samples_mut()[sample_index].offset = offset;
                    offset += self.tracks[track_index].samples()[sample_index].data.len() as u64;
                }
            }
        }

        // --- Second moov pass with real offsets.
        let moov = moov_box(&self.tracks, &chunks_per_track)?;
        debug_assert_eq!(moov.len(), moov_size);

        // --- Emit: ftyp + moov + mdat (faststart layout for progressive
        // playback — players can start reading moov before mdat is fully
        // downloaded).
        let mut out = Vec::with_capacity(ftyp.len() + moov.len() + 8 + mdat_payload.len());
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&moov);
        write_box_header(&mut out, b"mdat", 8 + mdat_payload.len())?;
        out.extend_from_slice(&mdat_payload);

        let infos = self.tracks.iter().map(Mp4Track::track_info).collect();
        Ok((out, infos))
    }
}

pub(crate) fn make_video_track(
    mut raw_samples: Vec<Mp4Sample>,
    sps: &[u8],
    pps: &[u8],
) -> Result<Mp4Track> {
    if raw_samples.is_empty() {
        return Err(Error::muxing("video track contains no samples"));
    }
    let info = avc::parse_sps(sps)?;
    let avcc = avc::avcc(sps, pps)?;
    assign_delta_durations(&mut raw_samples)?;

    Ok(Mp4Track::Video {
        samples: raw_samples,
        timescale: 90_000,
        width: info.width,
        height: info.height,
        codec: VideoCodec::Avc { avcc },
    })
}

pub(crate) fn make_hevc_video_track(
    mut raw_samples: Vec<Mp4Sample>,
    vps: &[u8],
    sps: &[u8],
    pps: &[u8],
) -> Result<Mp4Track> {
    use crate::codecs::hevc;
    if raw_samples.is_empty() {
        return Err(Error::muxing("video track contains no samples"));
    }
    let info = hevc::parse_sps(sps)?;
    let hvcc = hevc::hvcc(vps, sps, pps)?;
    assign_delta_durations(&mut raw_samples)?;

    Ok(Mp4Track::Video {
        samples: raw_samples,
        timescale: 90_000,
        width: info.width,
        height: info.height,
        codec: VideoCodec::Hevc { hvcc },
    })
}

pub(crate) fn make_audio_track(
    mut samples: Vec<Mp4Sample>,
    sample_rate: u32,
    channel_count: u8,
    audio_specific_config: Vec<u8>,
) -> Result<Mp4Track> {
    if samples.is_empty() {
        return Err(Error::muxing("audio track contains no samples"));
    }
    for sample in &mut samples {
        sample.duration = 1024;
    }

    Ok(Mp4Track::Audio {
        samples,
        timescale: sample_rate,
        sample_rate,
        channel_count,
        audio_specific_config,
    })
}

pub(crate) fn assign_delta_durations(samples: &mut [Mp4Sample]) -> Result<()> {
    samples.sort_by_key(|sample| sample.dts);
    let mut previous_delta = 3000_u32;
    for index in 0..samples.len() {
        let duration = if let Some(next) = samples.get(index + 1) {
            if next.dts <= samples[index].dts {
                return Err(Error::muxing(
                    "video sample DTS must be strictly increasing",
                ));
            }
            u32::try_from(next.dts - samples[index].dts)
                .map_err(|_| Error::muxing("video sample duration exceeds u32"))?
        } else {
            previous_delta
        };
        samples[index].duration = duration;
        previous_delta = duration;
    }
    Ok(())
}

fn track_duration(samples: &[Mp4Sample]) -> u64 {
    samples
        .last()
        .map(|sample| sample.dts + u64::from(sample.duration))
        .unwrap_or(0)
}

/// Presentation span in the track's timescale: max(PTS + duration) − min(PTS).
/// PTS-based (unlike `track_duration` which is DTS-based) so it reflects the
/// true visible duration, including the tail of B-frame reordering.
fn presentation_span(samples: &[Mp4Sample]) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let min_pts = samples.iter().map(|s| s.pts).min().unwrap_or(0);
    let max_end = samples
        .iter()
        .map(|s| s.pts + u64::from(s.duration))
        .max()
        .unwrap_or(0);
    max_end.saturating_sub(min_pts)
}

/// Initial PTS of the track in the track's timescale. Used to build an edit
/// list when the first sample doesn't start at PTS 0 (e.g. non-aligned HLS
/// segments).
fn start_offset(samples: &[Mp4Sample]) -> u64 {
    samples.first().map(|s| s.pts).unwrap_or(0)
}

fn ftyp_box(tracks: &[Mp4Track]) -> Vec<u8> {
    // Compatible brands are emitted conditionally based on the codecs that
    // actually appear in the file — writing `avc1`/`hvc1` for an audio-only
    // file is technically valid but misleading.
    let has_avc = tracks
        .iter()
        .any(|t| matches!(t, Mp4Track::Video { codec: VideoCodec::Avc { .. }, .. }));
    let has_hevc = tracks
        .iter()
        .any(|t| matches!(t, Mp4Track::Video { codec: VideoCodec::Hevc { .. }, .. }));
    boxed(b"ftyp", |out| {
        out.extend_from_slice(b"isom");
        be_u32(out, 0x200);
        out.extend_from_slice(b"isom");
        if has_avc {
            out.extend_from_slice(b"avc1");
        }
        if has_hevc {
            out.extend_from_slice(b"hvc1");
        }
        out.extend_from_slice(b"mp41");
    })
}

/// ftyp for fragmented MP4 / CMAF. Uses `iso5` as major brand because
/// QuickTime checks the major brand to decide whether the file is a
/// fragmented MP4 and whether to show playback controls.
fn fragmented_ftyp_box() -> Vec<u8> {
    boxed(b"ftyp", |out| {
        out.extend_from_slice(b"iso5");
        be_u32(out, 0x200);
        out.extend_from_slice(b"iso5");
        out.extend_from_slice(b"iso6");
        out.extend_from_slice(b"mp41");
    })
}

fn moov_box(tracks: &[Mp4Track], chunks_per_track: &[Vec<ChunkMeta>]) -> Result<Vec<u8>> {
    let movie_timescale = MOVIE_TIMESCALE;
    // Movie duration = max over tracks of (start_offset + presentation_span),
    // all rescaled into the movie timescale. Using PTS span (not DTS-based
    // track_duration) correctly accounts for B-frame composition offsets and
    // non-zero initial PTS.
    let movie_duration = tracks
        .iter()
        .map(|track| {
            let ts = track_timescale(track);
            let span = presentation_span(track.samples());
            let offset = start_offset(track.samples());
            rescale(span + offset, ts, movie_timescale)
        })
        .max()
        .unwrap_or(0);

    boxed_result(b"moov", |out| {
        out.extend_from_slice(&mvhd_box(movie_timescale, movie_duration, tracks.len())?);
        for (index, track) in tracks.iter().enumerate() {
            out.extend_from_slice(&trak_box(
                track,
                (index + 1) as u32,
                movie_timescale,
                &chunks_per_track[index],
            )?);
        }
        Ok(())
    })
}

fn trak_box(
    track: &Mp4Track,
    track_id: u32,
    movie_timescale: u32,
    chunks: &[ChunkMeta],
) -> Result<Vec<u8>> {
    let ts = track_timescale(track);
    let span = presentation_span(track.samples());
    let offset = start_offset(track.samples());
    // tkhd duration includes the initial PTS offset so the track's movie
    // timeline matches mvhd. mdhd duration uses just the presentation span
    // (media-local time, starting at 0).
    let tkhd_duration = rescale(span + offset, ts, movie_timescale);
    let offset_movie = rescale(offset, ts, movie_timescale);
    let span_movie = rescale(span, ts, movie_timescale);

    boxed_result(b"trak", |out| {
        out.extend_from_slice(&tkhd_box(track, track_id, tkhd_duration)?);
        // elst is only needed when the first sample's PTS > 0 (e.g. an HLS
        // segment that doesn't start at PTS 0). The standard "empty edit +
        // real edit" pair shifts the movie timeline past the initial gap.
        if offset_movie > 0 {
            out.extend_from_slice(&edts_box(offset_movie, span_movie)?);
        }
        out.extend_from_slice(&mdia_box(track, span, chunks)?);
        Ok(())
    })
}

/// Edit list box. Two entries: an empty edit (media_time = -1) covering the
/// initial PTS gap, followed by the real edit (media_time = 0) for the
/// presentation span. Per ISO/IEC 14496-12.
fn edts_box(empty_duration: u64, real_duration: u64) -> Result<Vec<u8>> {
    let elst = full_box_result(b"elst", 0, 0, |out| {
        be_u32(out, 2); // entry_count
        // Empty edit: hold for empty_duration, media_time = -1 (no media).
        be_u32(out, fit_u32(empty_duration, "edit list empty duration")?);
        be_i32(out, -1);
        be_u32(out, 0x0001_0000); // media_rate 1.0
        // Real edit: play the media for real_duration starting at media_time 0.
        be_u32(out, fit_u32(real_duration, "edit list segment duration")?);
        be_u32(out, 0);
        be_u32(out, 0x0001_0000);
        Ok(())
    })?;
    boxed_result(b"edts", |out| {
        out.extend_from_slice(&elst);
        Ok(())
    })
}

fn mdia_box(track: &Mp4Track, media_duration: u64, chunks: &[ChunkMeta]) -> Result<Vec<u8>> {
    boxed_result(b"mdia", |out| {
        out.extend_from_slice(&mdhd_box(track_timescale(track), media_duration)?);
        out.extend_from_slice(&hdlr_box(track));
        out.extend_from_slice(&minf_box(track, chunks)?);
        Ok(())
    })
}

fn minf_box(track: &Mp4Track, chunks: &[ChunkMeta]) -> Result<Vec<u8>> {
    boxed_result(b"minf", |out| {
        match track {
            Mp4Track::Video { .. } => out.extend_from_slice(&vmhd_box()),
            Mp4Track::Audio { .. } => out.extend_from_slice(&smhd_box()),
        }
        out.extend_from_slice(&dinf_box());
        out.extend_from_slice(&stbl_box(track, chunks)?);
        Ok(())
    })
}

fn stbl_box(track: &Mp4Track, chunks: &[ChunkMeta]) -> Result<Vec<u8>> {
    let needs_ctts = track
        .samples()
        .iter()
        .any(|sample| sample.pts != sample.dts);
    let all_key = track.samples().iter().all(|sample| sample.is_key);
    boxed_result(b"stbl", |out| {
        out.extend_from_slice(&stsd_box(track)?);
        out.extend_from_slice(&stts_box(track.samples()));
        if needs_ctts {
            out.extend_from_slice(&ctts_box(track.samples())?);
            // cslg helps players handle B-frame CTS offsets, especially
            // when some offsets are negative (QuickTime relies on this).
            out.extend_from_slice(&cslg_box(track.samples()));
        }
        // stss is omitted when every sample is a key frame — the spec
        // treats absence as "all sync", which is more compact.
        if matches!(track, Mp4Track::Video { .. }) && !all_key {
            out.extend_from_slice(&stss_box(track.samples()));
        }
        out.extend_from_slice(&stsc_box(chunks));
        out.extend_from_slice(&stsz_box(track.samples())?);
        out.extend_from_slice(&stco_box(track.samples(), chunks)?);
        Ok(())
    })
}

fn mvhd_box(timescale: u32, duration: u64, track_count: usize) -> Result<Vec<u8>> {
    let creation = creation_time();
    full_box_result(b"mvhd", 0, 0, |out| {
        be_u32(out, creation);
        be_u32(out, creation);
        be_u32(out, timescale);
        be_u32(out, fit_u32(duration, "movie duration")?);
        be_u32(out, 0x0001_0000);
        be_u16(out, 0x0100);
        be_u16(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        unity_matrix(out);
        for _ in 0..6 {
            be_u32(out, 0);
        }
        be_u32(out, fit_u32(track_count as u64 + 1, "next_track_id")?);
        Ok(())
    })
}

fn tkhd_box(track: &Mp4Track, track_id: u32, duration: u64) -> Result<Vec<u8>> {
    let creation = creation_time();
    full_box_result(b"tkhd", 0, 0x000007, |out| {
        be_u32(out, creation);
        be_u32(out, creation);
        be_u32(out, track_id);
        be_u32(out, 0);
        be_u32(out, fit_u32(duration, "track duration")?);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u16(out, 0);
        be_u16(out, 0);
        be_u16(
            out,
            if matches!(track, Mp4Track::Audio { .. }) {
                0x0100
            } else {
                0
            },
        );
        be_u16(out, 0);
        unity_matrix(out);
        match track {
            Mp4Track::Video { width, height, .. } => {
                be_u32(out, u32::from(*width) << 16);
                be_u32(out, u32::from(*height) << 16);
            }
            Mp4Track::Audio { .. } => {
                be_u32(out, 0);
                be_u32(out, 0);
            }
        }
        Ok(())
    })
}

fn mdhd_box(timescale: u32, duration: u64) -> Result<Vec<u8>> {
    let creation = creation_time();
    full_box_result(b"mdhd", 0, 0, |out| {
        be_u32(out, creation);
        be_u32(out, creation);
        be_u32(out, timescale);
        be_u32(out, fit_u32(duration, "media duration")?);
        be_u16(out, 0x55c4); // und
        be_u16(out, 0);
        Ok(())
    })
}

/// MP4 creation time as seconds since 1904-01-01 00:00:00 UTC (the MP4
/// epoch). Falls back to 0 if the wall clock is unavailable.
fn creation_time() -> u32 {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Seconds between 1904-01-01 and 1970-01-01.
    const EPOCH_OFFSET: u64 = 2_082_844_800;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| EPOCH_OFFSET.checked_add(d.as_secs()))
        .and_then(|s| u32::try_from(s).ok())
        .unwrap_or(0)
}

fn hdlr_box(track: &Mp4Track) -> Vec<u8> {
    full_box(b"hdlr", 0, 0, |out| {
        be_u32(out, 0);
        match track {
            Mp4Track::Video { .. } => out.extend_from_slice(b"vide"),
            Mp4Track::Audio { .. } => out.extend_from_slice(b"soun"),
        }
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        match track {
            Mp4Track::Video { .. } => out.extend_from_slice(b"VideoHandler\0"),
            Mp4Track::Audio { .. } => out.extend_from_slice(b"SoundHandler\0"),
        }
    })
}

fn vmhd_box() -> Vec<u8> {
    full_box(b"vmhd", 0, 1, |out| {
        be_u16(out, 0);
        be_u16(out, 0);
        be_u16(out, 0);
        be_u16(out, 0);
    })
}

fn smhd_box() -> Vec<u8> {
    full_box(b"smhd", 0, 0, |out| {
        be_u16(out, 0);
        be_u16(out, 0);
    })
}

fn dinf_box() -> Vec<u8> {
    boxed(b"dinf", |out| {
        out.extend_from_slice(&full_box(b"dref", 0, 0, |dref| {
            be_u32(dref, 1);
            dref.extend_from_slice(&full_box(b"url ", 0, 1, |_| {}));
        }));
    })
}

fn stsd_box(track: &Mp4Track) -> Result<Vec<u8>> {
    full_box_result(b"stsd", 0, 0, |out| {
        be_u32(out, 1);
        match track {
            Mp4Track::Video {
                width,
                height,
                codec,
                ..
            } => match codec {
                VideoCodec::Avc { avcc } => {
                    out.extend_from_slice(&avc1_entry(*width, *height, avcc)?)
                }
                VideoCodec::Hevc { hvcc } => {
                    out.extend_from_slice(&hvc1_entry(*width, *height, hvcc)?)
                }
            },
            Mp4Track::Audio {
                sample_rate,
                channel_count,
                audio_specific_config,
                ..
            } => out.extend_from_slice(&mp4a_entry(
                *sample_rate,
                *channel_count,
                audio_specific_config,
            )?),
        }
        Ok(())
    })
}

fn avc1_entry(width: u16, height: u16, avcc: &[u8]) -> Result<Vec<u8>> {
    boxed_result(b"avc1", |out| {
        out.extend_from_slice(&[0; 6]);
        be_u16(out, 1);
        be_u16(out, 0);
        be_u16(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u16(out, width);
        be_u16(out, height);
        be_u32(out, 0x0048_0000);
        be_u32(out, 0x0048_0000);
        be_u32(out, 0);
        be_u16(out, 1);
        out.push(0);
        out.extend_from_slice(&[0; 31]);
        be_u16(out, 0x0018);
        be_u16(out, 0xffff);
        out.extend_from_slice(&boxed(b"avcC", |box_out| box_out.extend_from_slice(avcc)));
        Ok(())
    })
}

fn hvc1_entry(width: u16, height: u16, hvcc: &[u8]) -> Result<Vec<u8>> {
    boxed_result(b"hvc1", |out| {
        out.extend_from_slice(&[0; 6]);
        be_u16(out, 1);
        be_u16(out, 0);
        be_u16(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u16(out, width);
        be_u16(out, height);
        be_u32(out, 0x0048_0000);
        be_u32(out, 0x0048_0000);
        be_u32(out, 0);
        be_u16(out, 1);
        out.push(0);
        out.extend_from_slice(&[0; 31]);
        be_u16(out, 0x0018);
        be_u16(out, 0xffff);
        out.extend_from_slice(&boxed(b"hvcC", |box_out| box_out.extend_from_slice(hvcc)));
        Ok(())
    })
}

fn mp4a_entry(sample_rate: u32, channel_count: u8, asc: &[u8]) -> Result<Vec<u8>> {
    boxed_result(b"mp4a", |out| {
        out.extend_from_slice(&[0; 6]);
        be_u16(out, 1);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u16(out, u16::from(channel_count));
        be_u16(out, 16);
        be_u16(out, 0);
        be_u16(out, 0);
        be_u32(out, sample_rate << 16);
        out.extend_from_slice(&esds_box(asc)?);
        Ok(())
    })
}

fn esds_box(asc: &[u8]) -> Result<Vec<u8>> {
    full_box_result(b"esds", 0, 0, |out| {
        let decoder_specific_len = asc.len();
        let decoder_config_len =
            13 + descriptor_len_size(decoder_specific_len) + decoder_specific_len;
        let es_len = 3 + descriptor_len_size(decoder_config_len) + decoder_config_len;

        descriptor(out, 0x03, es_len)?;
        be_u16(out, 1);
        out.push(0);

        descriptor(out, 0x04, decoder_config_len)?;
        out.push(0x40); // MPEG-4 Audio
        out.push(0x15); // AudioStream
        be_u24(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);

        descriptor(out, 0x05, decoder_specific_len)?;
        out.extend_from_slice(asc);

        descriptor(out, 0x06, 1)?;
        out.push(2);
        Ok(())
    })
}

fn descriptor(out: &mut Vec<u8>, tag: u8, len: usize) -> Result<()> {
    out.push(tag);
    write_descriptor_len(out, len)
}

fn descriptor_len_size(len: usize) -> usize {
    if len < 0x80 {
        1
    } else if len < 0x4000 {
        2
    } else if len < 0x20_0000 {
        3
    } else {
        4
    }
}

fn write_descriptor_len(out: &mut Vec<u8>, len: usize) -> Result<()> {
    if len >= 0x1000_0000 {
        return Err(Error::muxing("ES descriptor is too large"));
    }
    let size = descriptor_len_size(len);
    for index in (0..size).rev() {
        let mut byte = ((len >> (index * 7)) & 0x7f) as u8;
        if index != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
    Ok(())
}

fn stts_box(samples: &[Mp4Sample]) -> Vec<u8> {
    full_box(b"stts", 0, 0, |out| {
        let entries = grouped_counts(samples.iter().map(|sample| sample.duration));
        be_u32(out, entries.len() as u32);
        for (count, duration) in entries {
            be_u32(out, count);
            be_u32(out, duration);
        }
    })
}

fn ctts_box(samples: &[Mp4Sample]) -> Result<Vec<u8>> {
    // version 1: composition offsets are signed i32, supporting B-frame
    // scenarios where PTS < DTS (negative offset).
    full_box_result(b"ctts", 1, 0, |out| {
        let offsets: Vec<i32> = samples
            .iter()
            .map(|s| {
                let cts = s.pts as i64 - s.dts as i64;
                i32::try_from(cts)
                    .map_err(|_| Error::muxing("composition offset exceeds i32 range"))
            })
            .collect::<Result<_>>()?;
        let entries = grouped_counts(offsets.into_iter());
        be_u32(out, entries.len() as u32);
        for (count, offset) in entries {
            be_u32(out, count);
            be_i32(out, offset);
        }
        Ok(())
    })
}

/// Composition to Decode Box. Helps players (notably QuickTime) correctly
/// handle B-frame CTS offsets, especially when some offsets are negative.
/// version 0 uses i32 fields.
fn cslg_box(samples: &[Mp4Sample]) -> Vec<u8> {
    full_box(b"cslg", 0, 0, |out| {
        let cts_offsets: Vec<i64> = samples.iter().map(|s| s.pts as i64 - s.dts as i64).collect();
        let min_offset = *cts_offsets.iter().min().unwrap_or(&0);
        let max_offset = *cts_offsets.iter().max().unwrap_or(&0);
        // compositionToDtsShift: shift needed to keep all DTS non-negative
        // when CTS offsets are negative. = max(0, -min_offset).
        let composition_to_dts_shift = (-min_offset).max(0);
        let composition_start = samples.iter().map(|s| s.pts as i64).min().unwrap_or(0);
        let composition_end = samples
            .iter()
            .map(|s| s.pts as i64 + s.duration as i64)
            .max()
            .unwrap_or(0);
        be_i32(out, i32::try_from(composition_to_dts_shift).unwrap_or(i32::MAX));
        be_i32(out, i32::try_from(min_offset).unwrap_or(i32::MIN));
        be_i32(out, i32::try_from(max_offset).unwrap_or(i32::MAX));
        be_i32(out, i32::try_from(composition_start).unwrap_or(i32::MIN));
        be_i32(out, i32::try_from(composition_end).unwrap_or(i32::MAX));
    })
}

fn stss_box(samples: &[Mp4Sample]) -> Vec<u8> {
    full_box(b"stss", 0, 0, |out| {
        let key_indices: Vec<_> = samples
            .iter()
            .enumerate()
            .filter_map(|(index, sample)| sample.is_key.then_some((index + 1) as u32))
            .collect();
        be_u32(out, key_indices.len() as u32);
        for index in key_indices {
            be_u32(out, index);
        }
    })
}

/// One chunk's layout within a track. A chunk is a contiguous run of samples
/// in the mdat; grouping samples into ~0.5s chunks keeps stsc/stco compact
/// (one entry per chunk) instead of one entry per sample.
#[derive(Debug, Clone, Copy)]
struct ChunkMeta {
    /// Index into `track.samples()` of the first sample in this chunk.
    first_sample: usize,
    /// Number of samples in this chunk.
    sample_count: u32,
}

fn stsc_box(chunks: &[ChunkMeta]) -> Vec<u8> {
    // Compact encoding: merge runs of adjacent chunks that share the same
    // sample_count into a single stsc entry.
    let mut entries: Vec<(u32, u32, u32)> = Vec::new(); // (first_chunk, samples_per_chunk, desc_idx)
    for (index, chunk) in chunks.iter().enumerate() {
        if let Some((_, spc, _)) = entries.last_mut()
            && *spc == chunk.sample_count
        {
            continue;
        }
        entries.push(((index + 1) as u32, chunk.sample_count, 1));
    }
    full_box(b"stsc", 0, 0, |out| {
        be_u32(out, entries.len() as u32);
        for (first_chunk, spc, desc_idx) in entries {
            be_u32(out, first_chunk);
            be_u32(out, spc);
            be_u32(out, desc_idx);
        }
    })
}

fn stsz_box(samples: &[Mp4Sample]) -> Result<Vec<u8>> {
    full_box_result(b"stsz", 0, 0, |out| {
        be_u32(out, 0);
        be_u32(out, samples.len() as u32);
        for sample in samples {
            be_u32(out, fit_u32(sample.data.len() as u64, "sample size")?);
        }
        Ok(())
    })
}

fn stco_box(samples: &[Mp4Sample], chunks: &[ChunkMeta]) -> Result<Vec<u8>> {
    full_box_result(b"stco", 0, 0, |out| {
        be_u32(out, chunks.len() as u32);
        for chunk in chunks {
            let offset = samples[chunk.first_sample].offset;
            be_u32(out, fit_u32(offset, "chunk offset")?);
        }
        Ok(())
    })
}

fn grouped_counts<I, T>(values: I) -> Vec<(u32, T)>
where
    I: IntoIterator<Item = T>,
    T: PartialEq + Copy,
{
    let mut entries: Vec<(u32, T)> = Vec::new();
    for value in values {
        if let Some((count, last)) = entries.last_mut()
            && *last == value
        {
            *count += 1;
            continue;
        }
        entries.push((1, value));
    }
    entries
}

fn track_timescale(track: &Mp4Track) -> u32 {
    match track {
        Mp4Track::Video { timescale, .. } | Mp4Track::Audio { timescale, .. } => *timescale,
    }
}

/// Splits a track's samples into chunks of roughly 0.5 seconds each. Sample
/// durations are in the track's own timescale. The final chunk absorbs any
/// remaining samples even if shorter than the threshold.
fn split_chunks(track: &Mp4Track) -> Vec<ChunkMeta> {
    let samples = track.samples();
    if samples.is_empty() {
        return Vec::new();
    }
    let track_ts = track_timescale(track);
    // 0.5s in the track's timescale. Use saturating mul to avoid overflow on
    // unusually large timescales; fall back to a single chunk if so.
    let threshold = (track_ts as u64).saturating_mul(500) / 1000;

    let mut chunks = Vec::new();
    let mut current = ChunkMeta {
        first_sample: 0,
        sample_count: 0,
    };
    let mut current_duration: u64 = 0;
    for (index, sample) in samples.iter().enumerate() {
        if current_duration >= threshold && current.sample_count > 0 {
            chunks.push(current);
            current = ChunkMeta {
                first_sample: index,
                sample_count: 0,
            };
            current_duration = 0;
        }
        current.sample_count += 1;
        current_duration += u64::from(sample.duration);
    }
    if current.sample_count > 0 {
        chunks.push(current);
    }
    chunks
}

fn rescale(value: u64, from: u32, to: u32) -> u64 {
    value.saturating_mul(u64::from(to)) / u64::from(from)
}

fn fit_u32(value: u64, field: &str) -> Result<u32> {
    u32::try_from(value).map_err(|_| Error::muxing(format!("{field} exceeds u32")))
}

fn boxed(kind: &[u8; 4], write: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut payload = Vec::new();
    write(&mut payload);
    let mut out = Vec::with_capacity(8 + payload.len());
    write_box_header(&mut out, kind, 8 + payload.len())
        .expect("box generated by boxed is too large");
    out.extend_from_slice(&payload);
    out
}

fn boxed_result(kind: &[u8; 4], write: impl FnOnce(&mut Vec<u8>) -> Result<()>) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    write(&mut payload)?;
    let mut out = Vec::with_capacity(8 + payload.len());
    write_box_header(&mut out, kind, 8 + payload.len())?;
    out.extend_from_slice(&payload);
    Ok(out)
}

fn full_box(kind: &[u8; 4], version: u8, flags: u32, write: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    boxed(kind, |out| {
        out.push(version);
        be_u24(out, flags);
        write(out);
    })
}

fn full_box_result(
    kind: &[u8; 4],
    version: u8,
    flags: u32,
    write: impl FnOnce(&mut Vec<u8>) -> Result<()>,
) -> Result<Vec<u8>> {
    boxed_result(kind, |out| {
        out.push(version);
        be_u24(out, flags);
        write(out)
    })
}

fn write_box_header(out: &mut Vec<u8>, kind: &[u8; 4], size: usize) -> Result<()> {
    let size = u32::try_from(size).map_err(|_| Error::muxing("box exceeds u32 size"))?;
    be_u32(out, size);
    out.extend_from_slice(kind);
    Ok(())
}

fn unity_matrix(out: &mut Vec<u8>) {
    be_u32(out, 0x0001_0000);
    be_u32(out, 0);
    be_u32(out, 0);
    be_u32(out, 0);
    be_u32(out, 0x0001_0000);
    be_u32(out, 0);
    be_u32(out, 0);
    be_u32(out, 0);
    be_u32(out, 0x4000_0000);
}

fn be_u16(out: &mut Vec<u8>, value: u16) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be_u24(out: &mut Vec<u8>, value: u32) {
    out.push(((value >> 16) & 0xff) as u8);
    out.push(((value >> 8) & 0xff) as u8);
    out.push((value & 0xff) as u8);
}

fn be_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be_i32(out: &mut Vec<u8>, value: i32) {
    out.extend_from_slice(&value.to_be_bytes());
}

fn be_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_be_bytes());
}

#[derive(Debug, Clone)]
pub(crate) struct FragmentedTrack {
    pub track_id: u32,
    pub timescale: u32,
    pub kind: FragmentedTrackKind,
}

#[derive(Debug, Clone)]
pub(crate) enum FragmentedTrackKind {
    Video {
        width: u16,
        height: u16,
        codec: VideoCodec,
    },
    Audio {
        sample_rate: u32,
        channel_count: u8,
        audio_specific_config: Vec<u8>,
    },
}

impl FragmentedTrack {
    pub(crate) fn avc_video(track_id: u32, sps: &[u8], pps: &[u8]) -> Result<Self> {
        let info = avc::parse_sps(sps)?;
        let avcc = avc::avcc(sps, pps)?;
        Ok(Self {
            track_id,
            timescale: 90_000,
            kind: FragmentedTrackKind::Video {
                width: info.width,
                height: info.height,
                codec: VideoCodec::Avc { avcc },
            },
        })
    }

    pub(crate) fn hevc_video(track_id: u32, vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Self> {
        use crate::codecs::hevc;
        let info = hevc::parse_sps(sps)?;
        let hvcc = hevc::hvcc(vps, sps, pps)?;
        Ok(Self {
            track_id,
            timescale: 90_000,
            kind: FragmentedTrackKind::Video {
                width: info.width,
                height: info.height,
                codec: VideoCodec::Hevc { hvcc },
            },
        })
    }

    pub(crate) fn audio(
        track_id: u32,
        sample_rate: u32,
        channel_count: u8,
        audio_specific_config: Vec<u8>,
    ) -> Self {
        Self {
            track_id,
            timescale: sample_rate,
            kind: FragmentedTrackKind::Audio {
                sample_rate,
                channel_count,
                audio_specific_config,
            },
        }
    }
}

pub(crate) struct FragmentedMp4Muxer {
    tracks: Vec<FragmentedTrack>,
    next_sequence: u32,
}

impl FragmentedMp4Muxer {
    pub(crate) fn new(tracks: Vec<FragmentedTrack>) -> Self {
        Self {
            tracks,
            next_sequence: 1,
        }
    }

    pub(crate) fn write_header(&self) -> Result<Vec<u8>> {
        let ftyp = fragmented_ftyp_box();
        // Fragmented moov uses duration=0; the real timeline is carried by
        // per-fragment tfdt/trun. Writing a non-zero duration here makes some
        // players (QuickTime) treat it as a hard cap and stall on seek.
        let moov = fragmented_moov_box(&self.tracks)?;
        let mut out = Vec::with_capacity(ftyp.len() + moov.len());
        out.extend_from_slice(&ftyp);
        out.extend_from_slice(&moov);
        Ok(out)
    }

    /// Writes one `styp` + `moof` + `mdat` fragment.
    ///
    /// `samples_per_track[i]` corresponds to `self.tracks[i]`. Tracks with no
    /// samples in this fragment should pass an empty slice.
    pub(crate) fn write_fragment(
        &mut self,
        samples_per_track: &[Vec<Mp4Sample>],
    ) -> Result<Vec<u8>> {
        if samples_per_track.len() != self.tracks.len() {
            return Err(Error::invalid(
                "samples_per_track length must match tracks length",
            ));
        }

        let styp = styp_box();
        let moof = self.fragment_moof_box(samples_per_track)?;
        let mdat_payload_size = samples_per_track
            .iter()
            .flat_map(|samples| samples.iter())
            .try_fold(0_u64, |acc, sample| {
                acc.checked_add(sample.data.len() as u64)
                    .ok_or_else(|| Error::muxing("fragment media data is too large"))
            })?;
        if mdat_payload_size + 8 >= u32::MAX as u64 {
            return Err(Error::unsupported(
                "fragments larger than 4 GiB are out of Phase 3 scope",
            ));
        }

        let mut out =
            Vec::with_capacity(styp.len() + moof.len() + 8 + mdat_payload_size as usize);
        out.extend_from_slice(&styp);
        out.extend_from_slice(&moof);
        write_box_header(&mut out, b"mdat", 8 + mdat_payload_size as usize)?;
        for samples in samples_per_track {
            for sample in samples {
                out.extend_from_slice(&sample.data);
            }
        }

        self.next_sequence = self
            .next_sequence
            .checked_add(1)
            .ok_or_else(|| Error::muxing("fragment sequence number overflowed"))?;
        Ok(out)
    }

    fn fragment_moof_box(&self, samples_per_track: &[Vec<Mp4Sample>]) -> Result<Vec<u8>> {
        let sequence = self.next_sequence;
        // Pre-compute moof size and per-track data_offsets so the trun's
        // data_offset field can point at the correct byte in the mdat payload.
        let mut moof_payload_size: usize = 16; // mfhd box
        for (index, _track) in self.tracks.iter().enumerate() {
            let samples = &samples_per_track[index];
            if samples.is_empty() {
                continue;
            }
            // tfhd (16) + tfdt (20) + trun (20 + 16 * sample_count) + traf header (8)
            moof_payload_size += 8 + 16 + 20 + 20 + 16 * samples.len();
        }
        let moof_size = 8 + moof_payload_size;
        let mdat_header_size = 8;

        let mut data_offset = moof_size + mdat_header_size;
        let moof = boxed_result(b"moof", |out| {
            out.extend_from_slice(&mfhd_box(sequence)?);
            for (index, track) in self.tracks.iter().enumerate() {
                let samples = &samples_per_track[index];
                if samples.is_empty() {
                    continue;
                }
                out.extend_from_slice(&traf_box(track, samples, data_offset as u32)?);
                data_offset += samples
                    .iter()
                    .map(|s| s.data.len())
                    .sum::<usize>();
            }
            Ok(())
        })?;
        // Sanity check: the generated moof matches our pre-computed size.
        debug_assert_eq!(moof.len(), moof_size);
        Ok(moof)
    }
}

fn styp_box() -> Vec<u8> {
    boxed(b"styp", |out| {
        out.extend_from_slice(b"msdh");
        be_u32(out, 0);
        out.extend_from_slice(b"msdh");
        out.extend_from_slice(b"iso2");
    })
}

fn fragmented_moov_box(tracks: &[FragmentedTrack]) -> Result<Vec<u8>> {
    let movie_timescale = 1000_u32;
    boxed_result(b"moov", |out| {
        out.extend_from_slice(&mvhd_box(movie_timescale, 0, tracks.len())?);
        for track in tracks {
            out.extend_from_slice(&fragmented_trak_box(track)?);
        }
        out.extend_from_slice(&mvex_box(tracks)?);
        Ok(())
    })
}

fn fragmented_trak_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    boxed_result(b"trak", |out| {
        out.extend_from_slice(&fragmented_tkhd_box(track)?);
        out.extend_from_slice(&fragmented_mdia_box(track)?);
        Ok(())
    })
}

fn fragmented_tkhd_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    full_box_result(b"tkhd", 0, 0x000007, |out| {
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, track.track_id);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        be_u16(out, 0);
        be_u16(out, 0);
        be_u16(
            out,
            if matches!(track.kind, FragmentedTrackKind::Audio { .. }) {
                0x0100
            } else {
                0
            },
        );
        be_u16(out, 0);
        unity_matrix(out);
        match &track.kind {
            FragmentedTrackKind::Video { width, height, .. } => {
                be_u32(out, u32::from(*width) << 16);
                be_u32(out, u32::from(*height) << 16);
            }
            FragmentedTrackKind::Audio { .. } => {
                be_u32(out, 0);
                be_u32(out, 0);
            }
        }
        Ok(())
    })
}

fn fragmented_mdia_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    boxed_result(b"mdia", |out| {
        out.extend_from_slice(&mdhd_box(track.timescale, 0)?);
        out.extend_from_slice(&fragmented_hdlr_box(track));
        out.extend_from_slice(&fragmented_minf_box(track)?);
        Ok(())
    })
}

fn fragmented_hdlr_box(track: &FragmentedTrack) -> Vec<u8> {
    full_box(b"hdlr", 0, 0, |out| {
        be_u32(out, 0);
        match &track.kind {
            FragmentedTrackKind::Video { .. } => out.extend_from_slice(b"vide"),
            FragmentedTrackKind::Audio { .. } => out.extend_from_slice(b"soun"),
        }
        be_u32(out, 0);
        be_u32(out, 0);
        be_u32(out, 0);
        match &track.kind {
            FragmentedTrackKind::Video { .. } => out.extend_from_slice(b"VideoHandler\0"),
            FragmentedTrackKind::Audio { .. } => out.extend_from_slice(b"SoundHandler\0"),
        }
    })
}

fn fragmented_minf_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    boxed_result(b"minf", |out| {
        match &track.kind {
            FragmentedTrackKind::Video { .. } => out.extend_from_slice(&vmhd_box()),
            FragmentedTrackKind::Audio { .. } => out.extend_from_slice(&smhd_box()),
        }
        out.extend_from_slice(&dinf_box());
        out.extend_from_slice(&fragmented_stbl_box(track)?);
        Ok(())
    })
}

fn fragmented_stbl_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    boxed_result(b"stbl", |out| {
        out.extend_from_slice(&fragmented_stsd_box(track)?);
        // Empty sample tables — the real tables live in moof/trun.
        out.extend_from_slice(&full_box(b"stts", 0, 0, |o| be_u32(o, 0)));
        out.extend_from_slice(&full_box(b"stsc", 0, 0, |o| be_u32(o, 0)));
        out.extend_from_slice(&full_box(b"stsz", 0, 0, |o| {
            be_u32(o, 0);
            be_u32(o, 0);
        }));
        out.extend_from_slice(&full_box(b"stco", 0, 0, |o| be_u32(o, 0)));
        Ok(())
    })
}

fn fragmented_stsd_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    full_box_result(b"stsd", 0, 0, |out| {
        be_u32(out, 1);
        match &track.kind {
            FragmentedTrackKind::Video {
                width,
                height,
                codec,
            } => match codec {
                VideoCodec::Avc { avcc } => {
                    out.extend_from_slice(&avc1_entry(*width, *height, avcc)?)
                }
                VideoCodec::Hevc { hvcc } => {
                    out.extend_from_slice(&hvc1_entry(*width, *height, hvcc)?)
                }
            },
            FragmentedTrackKind::Audio {
                sample_rate,
                channel_count,
                audio_specific_config,
            } => out.extend_from_slice(&mp4a_entry(
                *sample_rate,
                *channel_count,
                audio_specific_config,
            )?),
        }
        Ok(())
    })
}

fn mvex_box(tracks: &[FragmentedTrack]) -> Result<Vec<u8>> {
    boxed_result(b"mvex", |out| {
        for track in tracks {
            out.extend_from_slice(&trex_box(track)?);
        }
        Ok(())
    })
}

fn trex_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    full_box_result(b"trex", 0, 0, |out| {
        be_u32(out, track.track_id);
        be_u32(out, 1); // default_sample_description_index
        be_u32(out, 0); // default_sample_duration
        be_u32(out, 0); // default_sample_size
        be_u32(out, 0); // default_sample_flags
        Ok(())
    })
}

/// One entry in a Track Fragment Random Access table. Points at the first
/// sync sample of a fragment so players can seek without scanning moof boxes.
#[derive(Debug, Clone)]
pub(crate) struct TfraEntry {
    /// Presentation time of the first sample in this fragment, in the track's
    /// own timescale.
    pub time: u64,
    /// Absolute byte offset of the fragment's `moof` box from the start of
    /// the file.
    pub moof_offset: u64,
    pub traf_number: u32,
    pub trun_number: u32,
    pub sample_number: u32,
}

/// Movie Fragment Random Access Box. Written at the very end of the file so
/// players can locate sync samples by seeking from EOF. Contains one `tfra`
/// per track plus a trailing `mfro` whose size field equals the full `mfra`
/// length (the size is computed up-front so no post-write patching is needed).
pub(crate) fn mfra_box(
    tracks: &[FragmentedTrack],
    entries_per_track: &[Vec<TfraEntry>],
) -> Result<Vec<u8>> {
    let mut tfras = Vec::with_capacity(tracks.len());
    let mut total_size: usize = 8; // mfra box header
    for (index, track) in tracks.iter().enumerate() {
        let tfra = tfra_box(track.track_id, &entries_per_track[index])?;
        total_size += tfra.len();
        tfras.push(tfra);
    }
    total_size += 16; // mfro: 8 header + 4 version/flags + 4 size

    boxed_result(b"mfra", |out| {
        for tfra in &tfras {
            out.extend_from_slice(tfra);
        }
        out.extend_from_slice(&mfro_box(fit_u32(total_size as u64, "mfra size")?));
        Ok(())
    })
}

fn tfra_box(track_id: u32, entries: &[TfraEntry]) -> Result<Vec<u8>> {
    full_box_result(b"tfra", 1, 0, |out| {
        be_u32(out, track_id);
        // 26 reserved bits + 2-bit traf length + 2-bit trun length + 2-bit sample length.
        // 0x3F → all three fields are 32-bit.
        be_u32(out, 0x0000_003f);
        be_u32(out, fit_u32(entries.len() as u64, "tfra entry count")?);
        for entry in entries {
            be_u64(out, entry.time);
            be_u64(out, entry.moof_offset);
            be_u32(out, entry.traf_number);
            be_u32(out, entry.trun_number);
            be_u32(out, entry.sample_number);
        }
        Ok(())
    })
}

fn mfro_box(size: u32) -> Vec<u8> {
    full_box(b"mfro", 0, 0, |out| {
        be_u32(out, size);
    })
}

fn mfhd_box(sequence_number: u32) -> Result<Vec<u8>> {
    full_box_result(b"mfhd", 0, 0, |out| {
        be_u32(out, sequence_number);
        Ok(())
    })
}

fn traf_box(
    track: &FragmentedTrack,
    samples: &[Mp4Sample],
    data_offset: u32,
) -> Result<Vec<u8>> {
    let base_decode_time = samples.first().map(|s| s.dts).unwrap_or(0);
    boxed_result(b"traf", |out| {
        out.extend_from_slice(&tfhd_box(track)?);
        out.extend_from_slice(&tfdt_box(base_decode_time)?);
        out.extend_from_slice(&trun_box(samples, data_offset)?);
        Ok(())
    })
}

fn tfhd_box(track: &FragmentedTrack) -> Result<Vec<u8>> {
    // default-base-is-moof (0x020000), sample-description-index-present absent
    full_box_result(b"tfhd", 0, 0x020_000, |out| {
        be_u32(out, track.track_id);
        Ok(())
    })
}

fn tfdt_box(base_decode_time: u64) -> Result<Vec<u8>> {
    full_box_result(b"tfdt", 1, 0, |out| {
        be_u64(out, base_decode_time);
        Ok(())
    })
}

fn trun_box(samples: &[Mp4Sample], data_offset: u32) -> Result<Vec<u8>> {
    // data-offset-present (0x000001), sample-duration-present (0x000100),
    // sample-size-present (0x000200), sample-flags-present (0x000400),
    // sample-composition-time-offset-present (0x000800); version=1 for signed cts
    let flags: u32 = 0x000_f01;
    full_box_result(b"trun", 1, flags, |out| {
        be_u32(out, samples.len() as u32);
        be_u32(out, data_offset);
        for sample in samples {
            be_u32(out, sample.duration);
            be_u32(out, fit_u32(sample.data.len() as u64, "fragment sample size")?);
            // Per ISO/IEC 14496-12 sample_flags bit layout:
            //   bits 25-24: sample_depends_on (2=independent/I-frame, 1=depends/P-B)
            //   bit 16: sample_is_non_sync_sample (0 for SAP, 1 for non-sync)
            // Key frame: 0x02000000 (independent + sync)
            // Delta frame: 0x01010000 (depends + non-sync)
            let sample_flags = if sample.is_key { 0x0200_0000 } else { 0x0101_0000 };
            be_u32(out, sample_flags);
            // composition offset (signed, i32 reinterpreted as u32 for wire format)
            let cts = sample.pts as i64 - sample.dts as i64;
            be_u32(out, cts as i32 as u32);
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_ftyp_box() {
        // ftyp_box inspects track codecs to choose compatible brands; an
        // empty slice yields just isom + mp41.
        let ftyp = ftyp_box(&[]);
        assert_eq!(&ftyp[4..8], b"ftyp");
        assert_eq!(&ftyp[8..12], b"isom");
    }

    #[test]
    fn groups_stts_entries() {
        let samples = vec![
            sample(0, 0, 100),
            sample(100, 100, 100),
            sample(200, 200, 120),
        ];
        let stts = stts_box(&samples);
        assert_eq!(&stts[4..8], b"stts");
        assert_eq!(u32::from_be_bytes(stts[12..16].try_into().unwrap()), 2);
    }

    #[test]
    fn fragmented_muxer_emits_init_and_fragment() {
        // Minimal fake "AVC" config: a tiny SPS NAL (type 7) + PPS NAL (type 8).
        // SPS bytes: [nal_header=0x67, profile=1, compat=2, level=3, then a minimal
        // but parseable SPS body] — wide/height are read via Exp-Golomb.
        // We avoid parse_sps here by constructing FragmentedTrack via the enum
        // directly so this test stays focused on the muxer box layout.
        let track = FragmentedTrack {
            track_id: 1,
            timescale: 90_000,
            kind: FragmentedTrackKind::Video {
                width: 320,
                height: 180,
                codec: VideoCodec::Avc { avcc: vec![0x01] },
            },
        };
        let mut muxer = FragmentedMp4Muxer::new(vec![track]);

        let header = muxer.write_header().unwrap();
        // ftyp + moov
        assert_eq!(&header[4..8], b"ftyp");
        let moov_pos = header
            .windows(4)
            .position(|w| w == b"moov")
            .expect("moov box present");
        assert_eq!(&header[moov_pos..moov_pos + 4], b"moov");
        // mvex present
        assert!(header.windows(4).any(|w| w == b"mvex"));

        let samples = vec![sample(0, 0, 1000), sample(1000, 1000, 1000)];
        let fragment = muxer.write_fragment(&[samples]).unwrap();
        // styp + moof + mdat
        assert_eq!(&fragment[4..8], b"styp");
        assert!(fragment.windows(4).any(|w| w == b"moof"));
        assert!(fragment.windows(4).any(|w| w == b"mdat"));
        // second fragment gets sequence number 2
        let _fragment2 = muxer.write_fragment(&[vec![sample(0, 0, 1000)]]).unwrap();
    }

    fn sample(dts: u64, pts: u64, duration: u32) -> Mp4Sample {
        Mp4Sample {
            data: vec![1, 2, 3],
            dts,
            pts,
            duration,
            is_key: true,
            offset: 0,
        }
    }
}
