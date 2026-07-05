//! A small, dependency-light HLS → MP4 transmuxer for Rust.
//!
//! `hls-transmux` reads an HLS playlist (local file or HTTP/HTTPS URL), demuxes
//! the underlying transport streams or fMP4/CMAF segments without re-encoding,
//! and writes a single MP4 output — no decoding, no encoding, no transcoding.
//!
//! # Quick start
//!
//! Local VOD playlist:
//!
//! ```no_run
//! use hls_transmux::{
//!     HlsInput, TransmuxOptions, transmux_hls_to_mp4_async,
//! };
//!
//! # async fn run() -> hls_transmux::Result<()> {
//! let report = transmux_hls_to_mp4_async(
//!     HlsInput::Path("playlist.m3u8".into()),
//!     "output.mp4",
//!     TransmuxOptions::default(),
//! )
//! .await?;
//! println!("wrote {} bytes across {} segments",
//!     report.bytes_written, report.segment_count);
//! # Ok(())
//! # }
//! ```
//!
//! HTTP master playlist with explicit variant selection, written as a
//! fragmented MP4 (CMAF-style `moof`/`mdat` per segment):
//!
//! ```no_run
//! use hls_transmux::{
//!     HlsInput, OutputFormat, TransmuxOptions, VariantSelection,
//!     transmux_hls_to_mp4_async,
//! };
//!
//! # async fn run() -> hls_transmux::Result<()> {
//! let report = transmux_hls_to_mp4_async(
//!     HlsInput::Url("https://example.com/master.m3u8".to_string()),
//!     "output.fmp4",
//!     TransmuxOptions {
//!         variant: Some(VariantSelection::Index(0)),
//!         output_format: OutputFormat::FragmentedMp4,
//!         ..Default::default()
//!     },
//! )
//! .await?;
//! # Ok(())
//! # }
//! ```
//!
//! For blocking call sites, wrap the async call with a runtime:
//!
//! ```no_run
//! use hls_transmux::{
//!     HlsInput, TransmuxOptions, transmux_hls_to_mp4_async,
//! };
//!
//! let report = tokio::runtime::Runtime::new()
//!     .unwrap()
//!     .block_on(transmux_hls_to_mp4_async(
//!         HlsInput::Path("playlist.m3u8".into()),
//!         "output.mp4",
//!         TransmuxOptions::default(),
//!     ))
//!     .unwrap();
//! ```
//!
//! # Supported inputs
//!
//! - HLS media playlists and master playlists (with explicit variant index).
//! - Local file paths and HTTP/HTTPS sources.
//! - Segment formats: MPEG-TS and fragmented MP4 / CMAF (`#EXT-X-MAP`).
//! - Video codecs: H.264/AVC and H.265/HEVC.
//! - Audio codec: AAC-LC.
//! - `#EXT-X-BYTERANGE` for both segments and init segments.
//!
//! The built-in `ReqwestSource` (enabled by the `default-source` cargo
//! feature, on by default) handles local files and HTTP/HTTPS. To plug in a
//! different HTTP client, cache, proxy, or fully offline source, implement
//! [`Source`] and pass it via [`HlsInput::custom`].
//!
//! # Supported outputs
//!
//! - [`Mp4`](OutputFormat::Mp4): non-fragmented MP4 (`ftyp` + `moov` + `mdat`), batch pipeline.
//! - [`FragmentedMp4`](OutputFormat::FragmentedMp4): fragmented MP4 / CMAF
//!   (`ftyp` + `moov` + per-segment `moof` + `mdat`), streaming pipeline.
//! - [`StreamingMp4`](OutputFormat::StreamingMp4): non-fragmented MP4 via the
//!   streaming fragmented pipeline + finalization. Same output as `Mp4`, lower
//!   peak memory; the temp file is a playable fMP4 if interrupted.
//!
//! # Not yet supported
//!
//! Encryption (AES-128 / SAMPLE-AES), live playlists, discontinuities, alternate
//! audio groups, multi-track streams, and non-AVC/HEVC/AAV-LC codecs return a
//! structured [`Error::Unsupported`] variant.

mod cancel;
mod codecs;
#[cfg(feature = "ffmpeg-finalize")]
mod ffmpeg_finalize;
mod error;
mod hls;
mod isobmff;
mod mp4;
mod mpeg_ts;
mod resume;
mod source;
mod transmux;
mod types;

pub use cancel::CancelToken;
pub use error::{Error, Result};
pub use resume::TransmuxResumeState;
pub use source::{ByteRange, HlsInput, Source, SourceLocation, TextResource};
#[cfg(feature = "default-source")]
pub use source::ReqwestSource;
pub use transmux::{
    FinalizeBackend, OutputFormat, TransmuxOptions, TransmuxProgress, VariantSelection,
    transmux_hls_to_mp4_async, transmux_hls_to_writer_async,
};
pub use types::{Codec, TrackInfo, TrackType, TransmuxReport};
