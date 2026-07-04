/// The kind of an output MP4 track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackType {
    Video,
    Audio,
}

/// A codec carried by an output track.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    /// H.264 / AVC.
    Avc,
    /// AAC (currently AAC-LC only).
    Aac,
    /// H.265 / HEVC.
    Hevc,
}

/// Metadata describing one track of the produced MP4.
///
/// Returned as part of a [`TransmuxReport`]. `duration` is expressed in
/// `timescale` units (milliseconds for the movie timescale used by this crate).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackInfo {
    pub track_type: TrackType,
    pub codec: Codec,
    pub timescale: u32,
    pub duration: u64,
    pub sample_count: usize,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub sample_rate: Option<u32>,
    pub channel_count: Option<u8>,
}

/// Summary of a completed HLS → MP4 transmux.
///
/// `duration` uses the units of `duration_timescale` (milliseconds unless noted
/// otherwise). For fragmented output, `tracks` is currently empty and only the
/// segment count, byte count, and duration are reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransmuxReport {
    /// Number of HLS media segments processed.
    pub segment_count: usize,
    /// Per-track metadata. Empty for fragmented output.
    pub tracks: Vec<TrackInfo>,
    /// Total duration in `duration_timescale` units.
    pub duration: u64,
    /// Timescale of `duration` (milliseconds for this crate).
    pub duration_timescale: u32,
    /// Bytes written to the output file.
    pub bytes_written: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Avc,
    Aac,
    Hevc,
}

#[derive(Debug, Clone)]
pub(crate) struct EncodedPacket {
    pub kind: StreamKind,
    pub data: Vec<u8>,
    pub pts_90k: u64,
    pub dts_90k: u64,
    pub duration: u64,
    pub is_key: bool,
    pub is_length_prefixed: bool,
}

/// Unified demux output shared by the MPEG-TS and ISOBMFF demuxers.
#[derive(Debug, Clone, Default)]
pub(crate) struct DemuxOutput {
    pub packets: Vec<EncodedPacket>,
    pub saw_video: bool,
    pub saw_audio: bool,
    pub vps: Option<Vec<u8>>,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub audio_specific_config: Option<Vec<u8>>,
    pub sample_rate: Option<u32>,
    pub channel_count: Option<u8>,
}
