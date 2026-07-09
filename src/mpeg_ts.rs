use std::collections::HashMap;

use crate::codecs::{aac, avc, hevc};
use crate::error::{Error, Result};
use crate::types::{DemuxOutput, EncodedPacket, StreamKind};

const TS_PACKET_SIZE: usize = 188;
const STREAM_TYPE_AAC: u8 = 0x0f;
const STREAM_TYPE_AVC: u8 = 0x1b;
const STREAM_TYPE_HEVC: u8 = 0x24;

#[derive(Debug, Clone)]
struct PesAccumulator {
    kind: StreamKind,
    data: Vec<u8>,
}

#[derive(Debug, Clone, Copy)]
struct PesHeader {
    pts_90k: u64,
    dts_90k: u64,
    payload_start: usize,
}

pub(crate) fn demux_ts(data: &[u8]) -> Result<DemuxOutput> {
    if data.len() < TS_PACKET_SIZE || data[0] != 0x47 {
        return Err(Error::invalid(
            "MPEG-TS segment must start with a 188-byte sync packet",
        ));
    }
    if data.len() % TS_PACKET_SIZE != 0 {
        return Err(Error::unsupported(
            "only plain 188-byte MPEG-TS packets are supported in Phase 1",
        ));
    }

    let mut pmt_pid = None;
    let mut streams: HashMap<u16, StreamKind> = HashMap::new();
    let mut accumulators: HashMap<u16, PesAccumulator> = HashMap::new();
    let mut result = DemuxOutput::default();

    for packet in data.chunks_exact(TS_PACKET_SIZE) {
        if packet[0] != 0x47 {
            return Err(Error::bitstream("MPEG-TS sync byte mismatch"));
        }

        let payload_unit_start = packet[1] & 0x40 != 0;
        let pid = (((packet[1] & 0x1f) as u16) << 8) | packet[2] as u16;
        let adaptation_field_control = (packet[3] >> 4) & 0x03;
        if adaptation_field_control == 0 {
            return Err(Error::bitstream("invalid MPEG-TS adaptation field control"));
        }
        if adaptation_field_control == 2 {
            continue;
        }

        let mut offset = 4;
        if adaptation_field_control == 3 {
            if offset >= packet.len() {
                return Err(Error::bitstream("truncated MPEG-TS adaptation field"));
            }
            let adaptation_len = packet[offset] as usize;
            offset += 1 + adaptation_len;
        }
        if offset >= packet.len() {
            continue;
        }
        let payload = &packet[offset..];

        if pid == 0 {
            if payload_unit_start {
                pmt_pid = Some(parse_pat(payload)?);
            }
            continue;
        }

        if Some(pid) == pmt_pid {
            if payload_unit_start {
                let parsed = parse_pmt(payload)?;
                streams = parsed;
                validate_streams(&streams)?;
            }
            continue;
        }

        let Some(&kind) = streams.get(&pid) else {
            continue;
        };

        if payload_unit_start {
            // For video streams (AVC/HEVC), a new PES packet may be a
            // continuation of a NAL unit that was split across PES packets.
            // This happens when a large access unit (e.g. an IDR frame)
            // exceeds the PES packet size. The continuation PES packet's
            // elementary-stream payload does NOT start with an Annex B start
            // code (00 00 01 / 00 00 00 01) — it is raw NAL data that should
            // be appended to the previous accumulator to complete the NAL
            // unit. Without this, the continuation payload is dropped by
            // `nal_units_annex_b` (which finds no start codes), truncating
            // the NAL unit and corrupting the H.264/HEVC bitstream.
            let is_video = matches!(kind, StreamKind::Avc | StreamKind::Hevc);
            let is_continuation = is_video
                && accumulators.contains_key(&pid)
                && pes_starts_new_access_unit(payload) == Some(false);

            if is_continuation {
                // Append the ES payload (after the PES header) to the previous
                // accumulator. The PES header (PTS/DTS) of the continuation is
                // discarded — the access unit's timestamp comes from the first
                // PES packet.
                let header_data_length = payload[8] as usize;
                let es_start = 9 + header_data_length;
                if let Some(accumulator) = accumulators.get_mut(&pid) {
                    accumulator.data.extend_from_slice(&payload[es_start..]);
                }
            } else {
                if let Some(accumulator) = accumulators.remove(&pid) {
                    flush_pes(accumulator, &mut result)?;
                }
                accumulators.insert(
                    pid,
                    PesAccumulator {
                        kind,
                        data: payload.to_vec(),
                    },
                );
            }
        } else if let Some(accumulator) = accumulators.get_mut(&pid) {
            accumulator.data.extend_from_slice(payload);
        }
    }

    for accumulator in accumulators.into_values() {
        flush_pes(accumulator, &mut result)?;
    }

    if !result.saw_video {
        return Err(Error::unsupported(
            "MPEG-TS segment does not contain an H.264 or HEVC video stream",
        ));
    }
    if !result.saw_audio {
        return Err(Error::unsupported(
            "MPEG-TS segment does not contain a Phase 1 AAC audio stream",
        ));
    }

    result
        .packets
        .sort_by_key(|packet| (packet.dts_90k, packet.pts_90k));
    Ok(result)
}

fn validate_streams(streams: &HashMap<u16, StreamKind>) -> Result<()> {
    let video_count = streams
        .values()
        .filter(|&&kind| kind == StreamKind::Avc || kind == StreamKind::Hevc)
        .count();
    let audio_count = streams
        .values()
        .filter(|&&kind| kind == StreamKind::Aac)
        .count();
    if video_count > 1 {
        return Err(Error::unsupported(
            "multiple video streams are out of scope",
        ));
    }
    if audio_count > 1 {
        return Err(Error::unsupported(
            "multiple AAC audio streams are out of scope",
        ));
    }
    Ok(())
}

fn parse_pat(payload: &[u8]) -> Result<u16> {
    let section = psi_section(payload)?;
    if section.first().copied() != Some(0x00) {
        return Err(Error::bitstream("expected PAT table_id 0x00"));
    }
    let section_length = section_length(section)?;
    let end = 3 + section_length - 4;
    if section.len() < end || end < 8 {
        return Err(Error::bitstream("truncated PAT section"));
    }

    let mut program_count = 0;
    let mut pmt_pid = None;
    let mut pos = 8;
    while pos + 4 <= end {
        let program_number = u16::from_be_bytes([section[pos], section[pos + 1]]);
        let pid = (((section[pos + 2] & 0x1f) as u16) << 8) | section[pos + 3] as u16;
        if program_number != 0 {
            program_count += 1;
            pmt_pid = Some(pid);
        }
        pos += 4;
    }

    if program_count > 1 {
        return Err(Error::unsupported(
            "multiple MPEG-TS programs are out of Phase 1 scope",
        ));
    }
    pmt_pid.ok_or_else(|| Error::bitstream("PAT does not reference a PMT"))
}

fn parse_pmt(payload: &[u8]) -> Result<HashMap<u16, StreamKind>> {
    let section = psi_section(payload)?;
    if section.first().copied() != Some(0x02) {
        return Err(Error::bitstream("expected PMT table_id 0x02"));
    }
    let section_length = section_length(section)?;
    let end = 3 + section_length - 4;
    if section.len() < end || end < 12 {
        return Err(Error::bitstream("truncated PMT section"));
    }

    let program_info_length = (((section[10] & 0x0f) as usize) << 8) | section[11] as usize;
    let mut pos = 12 + program_info_length;
    let mut streams = HashMap::new();
    while pos + 5 <= end {
        let stream_type = section[pos];
        let elementary_pid = (((section[pos + 1] & 0x1f) as u16) << 8) | section[pos + 2] as u16;
        let es_info_length =
            (((section[pos + 3] & 0x0f) as usize) << 8) | section[pos + 4] as usize;
        match stream_type {
            STREAM_TYPE_AVC => {
                streams.insert(elementary_pid, StreamKind::Avc);
            }
            STREAM_TYPE_HEVC => {
                streams.insert(elementary_pid, StreamKind::Hevc);
            }
            STREAM_TYPE_AAC => {
                streams.insert(elementary_pid, StreamKind::Aac);
            }
            _ => {}
        }
        pos += 5 + es_info_length;
    }

    if streams.is_empty() {
        return Err(Error::unsupported(
            "PMT does not contain H.264/HEVC/AAC streams",
        ));
    }
    Ok(streams)
}

fn psi_section(payload: &[u8]) -> Result<&[u8]> {
    if payload.is_empty() {
        return Err(Error::bitstream("empty PSI payload"));
    }
    let pointer = payload[0] as usize;
    let start = 1 + pointer;
    if start >= payload.len() {
        return Err(Error::bitstream("PSI pointer points past payload"));
    }
    Ok(&payload[start..])
}

fn section_length(section: &[u8]) -> Result<usize> {
    if section.len() < 3 {
        return Err(Error::bitstream("truncated PSI section header"));
    }
    Ok((((section[1] & 0x0f) as usize) << 8) | section[2] as usize)
}

fn flush_pes(accumulator: PesAccumulator, result: &mut DemuxOutput) -> Result<()> {
    let header = parse_pes_header(&accumulator.data)?;
    let payload = &accumulator.data[header.payload_start..];
    match accumulator.kind {
        StreamKind::Avc => {
            let (sps, pps) = avc::extract_sps_pps(payload);
            if result.sps.is_none() {
                result.sps = sps;
                if let Some(ref sps) = result.sps {
                    let info = avc::parse_sps(sps)?;
                    result.width = Some(info.width);
                    result.height = Some(info.height);
                }
            }
            if result.pps.is_none() {
                result.pps = pps;
            }
            if avc::nal_units_annex_b(payload).is_empty() {
                return Ok(());
            }
            result.saw_video = true;
            result.packets.push(EncodedPacket {
                kind: StreamKind::Avc,
                data: payload.to_vec(),
                pts_90k: header.pts_90k,
                dts_90k: header.dts_90k,
                duration: 0,
                is_key: avc::contains_idr(payload),
                is_length_prefixed: false,
            });
        }
        StreamKind::Hevc => {
            let (vps, sps, pps) = hevc::extract_vps_sps_pps(payload);
            if result.vps.is_none() {
                result.vps = vps;
            }
            if result.sps.is_none() {
                result.sps = sps;
                if let Some(ref sps) = result.sps {
                    let info = hevc::parse_sps(sps)?;
                    result.width = Some(info.width);
                    result.height = Some(info.height);
                }
            }
            if result.pps.is_none() {
                result.pps = pps;
            }
            if avc::nal_units_annex_b(payload).is_empty() {
                return Ok(());
            }
            result.saw_video = true;
            result.packets.push(EncodedPacket {
                kind: StreamKind::Hevc,
                data: payload.to_vec(),
                pts_90k: header.pts_90k,
                dts_90k: header.dts_90k,
                duration: 0,
                is_key: hevc::contains_irap(payload),
                is_length_prefixed: false,
            });
        }
        StreamKind::Aac => {
            let mut offset = 0;
            let mut frame_index = 0_u64;
            while offset < payload.len() {
                if payload.len() - offset < 7 {
                    break;
                }
                let header_adts = aac::parse_adts_header(&payload[offset..])?;
                if result.audio_specific_config.is_none() {
                    result.audio_specific_config = Some(aac::audio_specific_config(header_adts));
                    result.sample_rate = Some(header_adts.sample_rate);
                    result.channel_count = Some(header_adts.channel_config);
                } else if result.sample_rate != Some(header_adts.sample_rate)
                    || result.channel_count != Some(header_adts.channel_config)
                {
                    return Err(Error::unsupported(
                        "AAC stream parameter changes are out of Phase 1 scope",
                    ));
                }

                let frame_duration_90k = 1024_u64 * 90_000 / u64::from(header_adts.sample_rate);
                let pts = header.pts_90k + frame_index * frame_duration_90k;
                let frame_start = offset + header_adts.header_length;
                let frame_end = offset + header_adts.frame_length;
                result.saw_audio = true;
                result.packets.push(EncodedPacket {
                    kind: StreamKind::Aac,
                    data: payload[frame_start..frame_end].to_vec(),
                    pts_90k: pts,
                    dts_90k: pts,
                    duration: 1024,
                    is_key: true,
                    is_length_prefixed: false,
                });

                offset += header_adts.frame_length;
                frame_index += 1;
            }
        }
    }
    Ok(())
}

/// Checks if a PES packet's elementary-stream payload starts with an Annex B
/// start code (`00 00 01` or `00 00 00 01`).
///
/// Returns `None` if the PES header cannot be parsed (caller should fall back
/// to the default flush-and-restart behavior). Returns `Some(true)` if the ES
/// payload starts with a start code (new access unit). Returns `Some(false)` if
/// it does not (continuation of a NAL unit split across PES packets).
fn pes_starts_new_access_unit(payload: &[u8]) -> Option<bool> {
    // PES structure: 00 00 01 <stream_id> <len:2> <flags> <flags> <hdr_data_len> <optional> <ES payload>
    if payload.len() < 9 || payload[0..3] != [0x00, 0x00, 0x01] {
        return None;
    }
    let header_data_length = payload[8] as usize;
    let es_start = 9 + header_data_length;
    if es_start >= payload.len() {
        return None;
    }
    let es = &payload[es_start..];
    let has_start_code = (es.len() >= 4 && es[0] == 0 && es[1] == 0 && es[2] == 0 && es[3] == 1)
        || (es.len() >= 3 && es[0] == 0 && es[1] == 0 && es[2] == 1);
    Some(has_start_code)
}

fn parse_pes_header(data: &[u8]) -> Result<PesHeader> {
    if data.len() < 14 || data[0..3] != [0x00, 0x00, 0x01] {
        return Err(Error::bitstream("invalid PES packet header"));
    }
    let pts_dts_flags = (data[7] >> 6) & 0x03;
    let header_data_length = data[8] as usize;
    let payload_start = 9 + header_data_length;
    if data.len() < payload_start {
        return Err(Error::bitstream("truncated PES optional header"));
    }

    let pts = match pts_dts_flags {
        0b10 | 0b11 => parse_pes_timestamp(&data[9..14])?,
        _ => {
            return Err(Error::unsupported(
                "PES packets without PTS are out of Phase 1 scope",
            ));
        }
    };
    let dts = if pts_dts_flags == 0b11 {
        if data.len() < 19 {
            return Err(Error::bitstream("truncated PES DTS"));
        }
        parse_pes_timestamp(&data[14..19])?
    } else {
        pts
    };

    Ok(PesHeader {
        pts_90k: pts,
        dts_90k: dts,
        payload_start,
    })
}

fn parse_pes_timestamp(data: &[u8]) -> Result<u64> {
    if data.len() < 5 {
        return Err(Error::bitstream("truncated PES timestamp"));
    }
    Ok((((data[0] >> 1) & 0x07) as u64) << 30
        | ((u16::from_be_bytes([data[1], data[2]]) >> 1) as u64) << 15
        | ((u16::from_be_bytes([data[3], data[4]]) >> 1) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_non_ts_data() {
        let err = demux_ts(&[0; 188]).unwrap_err();
        assert!(matches!(err, Error::InvalidInput(_)));
    }

    #[test]
    fn parses_pes_timestamp() {
        let timestamp = 90_000_u64;
        let encoded = encode_pts(timestamp, 0b0010);
        assert_eq!(parse_pes_timestamp(&encoded).unwrap(), timestamp);
    }

    fn encode_pts(value: u64, prefix: u8) -> [u8; 5] {
        [
            (prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 1,
            (value >> 22) as u8,
            (((value >> 15) as u8 & 0x7f) << 1) | 1,
            (value >> 7) as u8,
            ((value as u8 & 0x7f) << 1) | 1,
        ]
    }
}
