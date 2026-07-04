use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::source::ByteRange;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InitSegment {
    pub uri: String,
    pub path: PathBuf,
    pub byte_range: Option<ByteRange>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HlsSegment {
    pub uri: String,
    pub path: PathBuf,
    pub duration_seconds: f64,
    pub sequence_number: u64,
    pub start_seconds: f64,
    pub byte_range: Option<ByteRange>,
    pub init_segment: Option<InitSegment>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaPlaylist {
    pub path: PathBuf,
    pub target_duration: Option<f64>,
    pub media_sequence: u64,
    pub segments: Vec<HlsSegment>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantStream {
    pub uri: String,
    pub path: PathBuf,
    pub bandwidth: Option<u64>,
    pub resolution: Option<Resolution>,
    pub codecs: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MasterPlaylist {
    pub path: PathBuf,
    pub variants: Vec<VariantStream>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum HlsPlaylist {
    Media(MediaPlaylist),
    Master(MasterPlaylist),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlaylistKind {
    Unknown,
    Media,
    Master,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingVariant {
    bandwidth: Option<u64>,
    resolution: Option<Resolution>,
    codecs: Option<String>,
}

pub(crate) fn parse_hls_playlist_content(
    path: Option<&Path>,
    content: &str,
) -> Result<HlsPlaylist> {
    let playlist_path = path.map_or_else(PathBuf::new, Path::to_path_buf);
    let base = path.and_then(Path::parent).unwrap_or_else(|| Path::new(""));
    let mut saw_header = false;
    let mut saw_endlist = false;
    let mut saw_vod_type = false;
    let mut target_duration = None;
    let mut media_sequence = 0_u64;
    let mut next_sequence = 0_u64;
    let mut next_duration = None;
    let mut next_byte_range = None;
    let mut previous_byte_range_end = 0_u64;
    let mut current_init_segment: Option<InitSegment> = None;
    let mut start_seconds = 0.0_f64;
    let mut segments = Vec::new();
    let mut variants = Vec::new();
    let mut kind = PlaylistKind::Unknown;
    let mut pending_variant = None;

    for (line_index, raw_line) in content.lines().enumerate() {
        let line_number = line_index + 1;
        let line = raw_line.trim();
        if line.is_empty() || (line.starts_with('#') && !line.starts_with("#EXT")) {
            continue;
        }

        if !saw_header {
            if line != "#EXTM3U" {
                return Err(Error::invalid("HLS playlist must start with #EXTM3U"));
            }
            saw_header = true;
            continue;
        }

        if let Some(value) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            ensure_kind(&mut kind, PlaylistKind::Master, line_number)?;
            let attributes = parse_attribute_list(value)?;
            pending_variant = Some(PendingVariant {
                bandwidth: parse_optional_u64(&attributes, "bandwidth", line_number)?,
                resolution: parse_resolution(attributes.get("resolution"), line_number)?,
                codecs: attributes.get("codecs").cloned(),
            });
            continue;
        }
        if line.starts_with("#EXT-X-MEDIA:") {
            return Err(Error::unsupported(
                "HLS alternate media groups are out of the Phase 2 slice",
            ));
        }
        if line.starts_with("#EXT-X-I-FRAME-STREAM-INF:") {
            return Err(Error::unsupported(
                "HLS I-frame-only variants are out of the Phase 2 slice",
            ));
        }
        if line.starts_with("#EXT-X-KEY:") {
            return Err(Error::unsupported(
                "encrypted HLS playlists are out of Phase 2 scope",
            ));
        }
        if let Some(value) = line.strip_prefix("#EXT-X-MAP:") {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            let attributes = parse_attribute_list(value)?;
            let uri = attributes
                .get("uri")
                .ok_or_else(|| {
                    Error::invalid(format!(
                        "#EXT-X-MAP on line {line_number} is missing URI attribute"
                    ))
                })?
                .clone();
            let byte_range = if let Some(range_value) = attributes.get("byterange") {
                Some(parse_byte_range(range_value, 0, line_number)?)
            } else {
                None
            };
            current_init_segment = Some(InitSegment {
                uri: uri.clone(),
                path: base.join(&uri),
                byte_range,
            });
            continue;
        }
        if line == "#EXT-X-DISCONTINUITY" {
            return Err(Error::unsupported(
                "HLS discontinuities are out of the Phase 2 slice",
            ));
        }
        if line.starts_with("#EXT-X-PROGRAM-DATE-TIME:") {
            return Err(Error::unsupported(
                "program date time is out of Phase 2 scope",
            ));
        }

        if let Some(value) = line.strip_prefix("#EXTINF:") {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            let duration_str = value
                .split_once(',')
                .map_or(value, |(duration, _)| duration)
                .trim();
            let duration = duration_str.parse::<f64>().map_err(|_| {
                Error::invalid(format!("invalid #EXTINF duration on line {line_number}"))
            })?;
            if !duration.is_finite() || duration < 0.0 {
                return Err(Error::invalid(format!(
                    "invalid #EXTINF duration on line {line_number}"
                )));
            }
            next_duration = Some(duration);
        } else if let Some(value) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            next_byte_range = Some(parse_byte_range(
                value,
                previous_byte_range_end,
                line_number,
            )?);
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            let duration = value.trim().parse::<f64>().map_err(|_| {
                Error::invalid(format!(
                    "invalid #EXT-X-TARGETDURATION on line {line_number}"
                ))
            })?;
            if !duration.is_finite() || duration < 0.0 {
                return Err(Error::invalid(format!(
                    "invalid #EXT-X-TARGETDURATION on line {line_number}"
                )));
            }
            target_duration = Some(duration);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            media_sequence = value.trim().parse::<u64>().map_err(|_| {
                Error::invalid(format!(
                    "invalid #EXT-X-MEDIA-SEQUENCE on line {line_number}"
                ))
            })?;
            next_sequence = media_sequence;
        } else if line == "#EXT-X-ENDLIST" {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            saw_endlist = true;
        } else if line.starts_with("#EXT-X-PLAYLIST-TYPE:") {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            let playlist_type = line["#EXT-X-PLAYLIST-TYPE:".len()..].trim();
            if playlist_type.eq_ignore_ascii_case("VOD") {
                saw_vod_type = true;
            } else {
                return Err(Error::unsupported(
                    "live/event playlists are out of Phase 2 scope",
                ));
            }
        } else if line.starts_with("#EXT-X-VERSION") {
            // Informational only — does not affect parsing.
        } else if line.starts_with("#EXT") {
            return Err(Error::unsupported(format!(
                "unsupported HLS tag on line {line_number}: {line}"
            )));
        } else if let Some(variant) = pending_variant.take() {
            ensure_kind(&mut kind, PlaylistKind::Master, line_number)?;
            variants.push(VariantStream {
                uri: line.to_owned(),
                path: base.join(line),
                bandwidth: variant.bandwidth,
                resolution: variant.resolution,
                codecs: variant.codecs,
            });
        } else {
            ensure_kind(&mut kind, PlaylistKind::Media, line_number)?;
            let duration = next_duration.take().ok_or_else(|| {
                Error::invalid(format!(
                    "segment URI on line {line_number} must be preceded by #EXTINF"
                ))
            })?;
            let byte_range = next_byte_range.take();
            if let Some(range) = &byte_range {
                previous_byte_range_end = range
                    .offset
                    .checked_add(range.length)
                    .ok_or_else(|| Error::invalid("HLS byterange offset overflows u64"))?;
            }
            segments.push(HlsSegment {
                uri: line.to_owned(),
                path: base.join(line),
                duration_seconds: duration,
                sequence_number: next_sequence,
                start_seconds,
                byte_range,
                init_segment: current_init_segment.clone(),
            });
            start_seconds += duration;
            next_sequence += 1;
        }
    }

    if !saw_header {
        return Err(Error::invalid("HLS playlist is missing #EXTM3U"));
    }
    if pending_variant.is_some() {
        return Err(Error::invalid(
            "#EXT-X-STREAM-INF must be followed by a variant URI",
        ));
    }

    match kind {
        PlaylistKind::Master => {
            if variants.is_empty() {
                return Err(Error::invalid("HLS master playlist contains no variants"));
            }
            Ok(HlsPlaylist::Master(MasterPlaylist {
                path: playlist_path,
                variants,
            }))
        }
        PlaylistKind::Media | PlaylistKind::Unknown => {
            if !saw_endlist && !saw_vod_type {
                return Err(Error::unsupported(
                    "live playlists without #EXT-X-ENDLIST are out of Phase 2 scope",
                ));
            }
            if segments.is_empty() {
                return Err(Error::invalid("HLS playlist contains no media segments"));
            }
            Ok(HlsPlaylist::Media(MediaPlaylist {
                path: playlist_path,
                target_duration,
                media_sequence,
                segments,
            }))
        }
    }
}

fn ensure_kind(kind: &mut PlaylistKind, next: PlaylistKind, line_number: usize) -> Result<()> {
    if *kind == PlaylistKind::Unknown {
        *kind = next;
        return Ok(());
    }
    if *kind != next {
        return Err(Error::invalid(format!(
            "HLS playlist mixes master and media tags near line {line_number}"
        )));
    }
    Ok(())
}

fn parse_attribute_list(value: &str) -> Result<HashMap<String, String>> {
    let mut attributes = HashMap::new();
    let bytes = value.as_bytes();
    let mut start = 0;
    let mut in_quotes = false;

    for index in 0..=bytes.len() {
        if index < bytes.len() && bytes[index] == b'"' {
            in_quotes = !in_quotes;
        }
        if index == bytes.len() || (bytes[index] == b',' && !in_quotes) {
            let part = value[start..index].trim();
            if !part.is_empty() {
                let (key, raw_value) = part
                    .split_once('=')
                    .ok_or_else(|| Error::invalid(format!("invalid HLS attribute: {part}")))?;
                let normalized = raw_value.trim().trim_matches('"').to_owned();
                attributes.insert(key.trim().to_ascii_lowercase(), normalized);
            }
            start = index + 1;
        }
    }

    if in_quotes {
        return Err(Error::invalid("unterminated quote in HLS attribute list"));
    }

    Ok(attributes)
}

fn parse_optional_u64(
    attributes: &HashMap<String, String>,
    name: &str,
    line_number: usize,
) -> Result<Option<u64>> {
    attributes
        .get(name)
        .map(|value| {
            value.parse::<u64>().map_err(|_| {
                Error::invalid(format!("invalid {name} attribute on line {line_number}"))
            })
        })
        .transpose()
}

fn parse_resolution(value: Option<&String>, line_number: usize) -> Result<Option<Resolution>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let (width, height) = value.split_once('x').ok_or_else(|| {
        Error::invalid(format!(
            "invalid resolution attribute on line {line_number}"
        ))
    })?;
    Ok(Some(Resolution {
        width: width.parse::<u32>().map_err(|_| {
            Error::invalid(format!("invalid resolution width on line {line_number}"))
        })?,
        height: height.parse::<u32>().map_err(|_| {
            Error::invalid(format!("invalid resolution height on line {line_number}"))
        })?,
    }))
}

fn parse_byte_range(value: &str, previous_end: u64, line_number: usize) -> Result<ByteRange> {
    let (length, offset) = value
        .trim()
        .split_once('@')
        .map_or((value.trim(), None), |(length, offset)| {
            (length.trim(), Some(offset.trim()))
        });
    let length = length.parse::<u64>().map_err(|_| {
        Error::invalid(format!(
            "invalid #EXT-X-BYTERANGE length on line {line_number}"
        ))
    })?;
    let offset = offset
        .map(|offset| {
            offset.parse::<u64>().map_err(|_| {
                Error::invalid(format!(
                    "invalid #EXT-X-BYTERANGE offset on line {line_number}"
                ))
            })
        })
        .transpose()?
        .unwrap_or(previous_end);
    Ok(ByteRange { offset, length })
}

#[cfg(test)]
fn parse_hls_playlist_str(path: &Path, content: &str) -> Result<HlsPlaylist> {
    parse_hls_playlist_content(Some(path), content)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_vod_playlist() {
        let playlist = parse_hls_playlist_str(
            Path::new("/tmp/media/playlist.m3u8"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:9\n#EXTINF:3.5,\nseg0.ts\n#EXTINF:4,\nseg1.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap();

        let HlsPlaylist::Media(playlist) = playlist else {
            panic!("expected media playlist");
        };
        assert_eq!(playlist.segments.len(), 2);
        assert_eq!(playlist.segments[0].sequence_number, 9);
        assert_eq!(
            playlist.segments[0].path,
            PathBuf::from("/tmp/media/seg0.ts")
        );
        assert_eq!(playlist.segments[1].start_seconds, 3.5);
    }

    #[test]
    fn parses_master_playlist_with_quoted_codecs() {
        let playlist = parse_hls_playlist_str(
            Path::new("/tmp/master.m3u8"),
            "#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=1280000,RESOLUTION=640x360,CODECS=\"avc1.42e01e,mp4a.40.2\"\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=2560000\nmid/index.m3u8\n",
        )
        .unwrap();

        let HlsPlaylist::Master(master) = playlist else {
            panic!("expected master playlist");
        };
        assert_eq!(master.variants.len(), 2);
        assert_eq!(master.variants[0].bandwidth, Some(1_280_000));
        assert_eq!(
            master.variants[0].codecs.as_deref(),
            Some("avc1.42e01e,mp4a.40.2")
        );
        assert_eq!(
            master.variants[0].resolution,
            Some(Resolution {
                width: 640,
                height: 360
            })
        );
    }

    #[test]
    fn parses_media_playlist_byteranges() {
        let playlist = parse_hls_playlist_str(
            Path::new("/tmp/media/playlist.m3u8"),
            "#EXTM3U\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXTINF:3,\n#EXT-X-BYTERANGE:20@10\nseg.ts\n#EXTINF:3,\n#EXT-X-BYTERANGE:5\nseg.ts\n",
        )
        .unwrap();

        let HlsPlaylist::Media(playlist) = playlist else {
            panic!("expected media playlist");
        };
        assert_eq!(
            playlist.segments[0].byte_range,
            Some(ByteRange {
                offset: 10,
                length: 20
            })
        );
        assert_eq!(
            playlist.segments[1].byte_range,
            Some(ByteRange {
                offset: 30,
                length: 5
            })
        );
    }

    #[test]
    fn rejects_alternate_media_playlist() {
        let err = parse_hls_playlist_str(
            Path::new("playlist.m3u8"),
            "#EXTM3U\n#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",URI=\"audio.m3u8\"\n",
        )
        .unwrap_err();
        assert!(matches!(err, Error::Unsupported(_)));
    }

    #[test]
    fn rejects_segment_without_duration() {
        let err = parse_hls_playlist_str(
            Path::new("playlist.m3u8"),
            "#EXTM3U\nseg0.ts\n#EXT-X-ENDLIST\n",
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn parses_ext_x_map_init_segment() {
        let playlist = parse_hls_playlist_str(
            Path::new("/tmp/media/playlist.m3u8"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:3,\nseg0.m4s\n#EXTINF:3,\nseg1.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();

        let HlsPlaylist::Media(playlist) = playlist else {
            panic!("expected media playlist");
        };
        assert_eq!(playlist.segments.len(), 2);
        let init0 = playlist.segments[0].init_segment.as_ref().unwrap();
        assert_eq!(init0.uri, "init.mp4");
        assert_eq!(init0.path, PathBuf::from("/tmp/media/init.mp4"));
        assert_eq!(init0.byte_range, None);
        // second segment reuses the current init segment
        assert_eq!(
            playlist.segments[1].init_segment.as_ref().unwrap().uri,
            "init.mp4"
        );
    }

    #[test]
    fn parses_ext_x_map_with_byterange() {
        let playlist = parse_hls_playlist_str(
            Path::new("/tmp/media/playlist.m3u8"),
            "#EXTM3U\n#EXT-X-TARGETDURATION:4\n#EXT-X-MAP:URI=\"init.mp4\",BYTERANGE=\"100@50\"\n#EXTINF:3,\nseg0.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap();

        let HlsPlaylist::Media(playlist) = playlist else {
            panic!("expected media playlist");
        };
        let init = playlist.segments[0].init_segment.as_ref().unwrap();
        assert_eq!(
            init.byte_range,
            Some(ByteRange {
                offset: 50,
                length: 100
            })
        );
    }

    #[test]
    fn rejects_ext_x_map_without_uri() {
        let err = parse_hls_playlist_str(
            Path::new("playlist.m3u8"),
            "#EXTM3U\n#EXT-X-MAP:BYTERANGE=\"10@0\"\n#EXTINF:3,\nseg0.m4s\n#EXT-X-ENDLIST\n",
        )
        .unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }
}
