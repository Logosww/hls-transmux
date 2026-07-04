/// Checkpoint for resuming an interrupted transmux. The crate attaches a
/// fresh snapshot to every [`TransmuxProgress`](crate::TransmuxProgress)
/// callback; callers persist it and pass it back via
/// [`TransmuxOptions::resume`](crate::TransmuxOptions::resume) to continue.
///
/// Only 4 fields are persisted:
/// - `tracks` (codec config) are re-extracted by re-demuxing `segments[0]`
///   on resume, avoiding serializing SPS/PPS/ASC bytes into the checkpoint
///   (more stable across versions).
/// - `mfra` box entries are rebuilt by scanning the existing `.partial.mp4`'s
///   moof boxes on resume, so resumed runs emit a complete `mfra` matching a
///   fresh run's output byte-for-byte.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct TransmuxResumeState {
    /// Number of segments already written to the output. Resume skips
    /// `segments[..completed_segments]`.
    pub completed_segments: usize,
    /// Current byte offset of the output file. The crate opens the file
    /// in append mode and continues from this offset.
    pub bytes_written: u64,
    /// Next fragment's `mfhd` sequence number (starts at 1, increments
    /// per fragment). Resume uses this directly as the first fragment's
    /// sequence.
    pub next_sequence: u32,
    /// First packet's DTS in 90 kHz domain. All sample timestamps are
    /// shifted by this base. Resume must reuse the original value or
    /// subsequent fragments' `tfdt` will be discontinuous.
    pub global_base_dts_90k: u64,
}
