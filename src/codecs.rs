pub(crate) mod bit_reader {
    use crate::error::{Error, Result};

    pub(crate) struct BitReader<'a> {
        data: &'a [u8],
        bit_pos: usize,
    }

    impl<'a> BitReader<'a> {
        pub(crate) fn new(data: &'a [u8]) -> Self {
            Self { data, bit_pos: 0 }
        }

        pub(crate) fn read_bit(&mut self) -> Result<bool> {
            if self.bit_pos / 8 >= self.data.len() {
                return Err(Error::bitstream("unexpected end of bitstream"));
            }
            let byte = self.data[self.bit_pos / 8];
            let bit = (byte >> (7 - (self.bit_pos % 8))) & 1;
            self.bit_pos += 1;
            Ok(bit != 0)
        }

        pub(crate) fn read_bits(&mut self, count: usize) -> Result<u32> {
            let mut value = 0_u32;
            for _ in 0..count {
                value = (value << 1) | u32::from(self.read_bit()?);
            }
            Ok(value)
        }

        pub(crate) fn read_ue(&mut self) -> Result<u32> {
            let mut zero_count = 0;
            while !self.read_bit()? {
                zero_count += 1;
                if zero_count > 31 {
                    return Err(Error::bitstream("Exp-Golomb value is too large"));
                }
            }
            let suffix = if zero_count == 0 {
                0
            } else {
                self.read_bits(zero_count)?
            };
            Ok((1_u32 << zero_count) - 1 + suffix)
        }

        pub(crate) fn read_se(&mut self) -> Result<i32> {
            let value = self.read_ue()? as i32;
            if value % 2 == 0 {
                Ok(-(value / 2))
            } else {
                Ok((value + 1) / 2)
            }
        }
    }
}

pub(crate) mod aac {
    use crate::error::{Error, Result};

    const SAMPLE_RATES: [u32; 13] = [
        96_000, 88_200, 64_000, 48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025,
        8_000, 7_350,
    ];

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct AdtsHeader {
        pub audio_object_type: u8,
        pub sample_rate: u32,
        pub sample_rate_index: u8,
        pub channel_config: u8,
        pub frame_length: usize,
        pub header_length: usize,
    }

    pub(crate) fn parse_adts_header(data: &[u8]) -> Result<AdtsHeader> {
        if data.len() < 7 {
            return Err(Error::bitstream("ADTS frame is shorter than 7 bytes"));
        }
        if data[0] != 0xff || (data[1] & 0xf0) != 0xf0 {
            return Err(Error::bitstream("missing ADTS syncword"));
        }

        let protection_absent = data[1] & 0x01 != 0;
        let profile = (data[2] & 0xc0) >> 6;
        let audio_object_type = profile + 1;
        if audio_object_type != 2 {
            return Err(Error::unsupported(
                "only AAC LC ADTS streams are supported in Phase 1",
            ));
        }

        let sample_rate_index = (data[2] & 0x3c) >> 2;
        let sample_rate = *SAMPLE_RATES
            .get(sample_rate_index as usize)
            .ok_or_else(|| Error::unsupported("unsupported ADTS sample rate index"))?;
        let channel_config = ((data[2] & 0x01) << 2) | ((data[3] & 0xc0) >> 6);
        if channel_config == 0 {
            return Err(Error::unsupported(
                "AAC program config elements are out of Phase 1 scope",
            ));
        }

        let frame_length = (((data[3] & 0x03) as usize) << 11)
            | ((data[4] as usize) << 3)
            | (((data[5] & 0xe0) as usize) >> 5);
        let header_length = if protection_absent { 7 } else { 9 };
        if frame_length < header_length {
            return Err(Error::bitstream("invalid ADTS frame length"));
        }
        if data.len() < frame_length {
            return Err(Error::bitstream("truncated ADTS frame"));
        }

        Ok(AdtsHeader {
            audio_object_type,
            sample_rate,
            sample_rate_index,
            channel_config,
            frame_length,
            header_length,
        })
    }

    pub(crate) fn audio_specific_config(header: AdtsHeader) -> Vec<u8> {
        vec![
            (header.audio_object_type << 3) | (header.sample_rate_index >> 1),
            ((header.sample_rate_index & 0x01) << 7) | (header.channel_config << 3),
        ]
    }

    #[cfg(test)]
    pub(crate) fn make_adts_header(payload_len: usize) -> [u8; 7] {
        let frame_len = payload_len + 7;
        [
            0xff,
            0xf1,
            0x50,
            0x80 | (((frame_len >> 11) as u8) & 0x03),
            ((frame_len >> 3) as u8) & 0xff,
            (((frame_len & 0x07) as u8) << 5) | 0x1f,
            0xfc,
        ]
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_adts_header_and_config() {
            let mut frame = make_adts_header(4).to_vec();
            frame.extend_from_slice(&[1, 2, 3, 4]);
            let header = parse_adts_header(&frame).unwrap();

            assert_eq!(header.audio_object_type, 2);
            assert_eq!(header.sample_rate, 44_100);
            assert_eq!(header.channel_config, 2);
            assert_eq!(header.frame_length, 11);
            assert_eq!(audio_specific_config(header), vec![0x12, 0x10]);
        }
    }
}

pub(crate) mod avc {
    use crate::codecs::bit_reader::BitReader;
    use crate::error::{Error, Result};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct SpsInfo {
        pub width: u16,
        pub height: u16,
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct NalUnit<'a> {
        pub data: &'a [u8],
    }

    pub(crate) fn nal_units_annex_b(data: &[u8]) -> Vec<NalUnit<'_>> {
        let mut starts = Vec::new();
        let mut i = 0;
        while i + 3 <= data.len() {
            let start_code_len = if i + 4 <= data.len()
                && data[i] == 0
                && data[i + 1] == 0
                && data[i + 2] == 0
                && data[i + 3] == 1
            {
                Some(4)
            } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                Some(3)
            } else {
                None
            };

            if let Some(len) = start_code_len {
                starts.push((i, i + len));
                i += len;
            } else {
                i += 1;
            }
        }

        let mut units = Vec::new();
        for index in 0..starts.len() {
            let payload_start = starts[index].1;
            let payload_end = starts.get(index + 1).map_or(data.len(), |next| next.0);
            let mut end = payload_end;
            while end > payload_start && data[end - 1] == 0 {
                end -= 1;
            }
            if payload_start < end {
                units.push(NalUnit {
                    data: &data[payload_start..end],
                });
            }
        }

        units
    }

    pub(crate) fn annex_b_to_length_prefixed(data: &[u8]) -> Result<Vec<u8>> {
        let units = nal_units_annex_b(data);
        if units.is_empty() {
            return Err(Error::bitstream(
                "Annex B packet does not contain NAL units",
            ));
        }

        let mut out = Vec::new();
        for unit in units {
            let len = u32::try_from(unit.data.len())
                .map_err(|_| Error::bitstream("NAL unit is too large"))?;
            out.extend_from_slice(&len.to_be_bytes());
            out.extend_from_slice(unit.data);
        }
        Ok(out)
    }

    pub(crate) fn contains_idr(data: &[u8]) -> bool {
        nal_units_annex_b(data)
            .into_iter()
            .any(|unit| !unit.data.is_empty() && (unit.data[0] & 0x1f) == 5)
    }

    pub(crate) fn extract_sps_pps(data: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
        let mut sps = None;
        let mut pps = None;
        for unit in nal_units_annex_b(data) {
            if unit.data.is_empty() {
                continue;
            }
            match unit.data[0] & 0x1f {
                7 if sps.is_none() => sps = Some(unit.data.to_vec()),
                8 if pps.is_none() => pps = Some(unit.data.to_vec()),
                _ => {}
            }
        }
        (sps, pps)
    }

    pub(crate) fn avcc(sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
        if sps.len() < 4 {
            return Err(Error::bitstream("SPS is too short to build avcC"));
        }

        let mut out = vec![1, sps[1], sps[2], sps[3], 0xff, 0xe1];
        push_u16(&mut out, sps.len())?;
        out.extend_from_slice(sps);
        out.push(1);
        push_u16(&mut out, pps.len())?;
        out.extend_from_slice(pps);
        Ok(out)
    }

    fn push_u16(out: &mut Vec<u8>, value: usize) -> Result<()> {
        let value = u16::try_from(value)
            .map_err(|_| Error::bitstream("AVC decoder config NAL unit is too large"))?;
        out.extend_from_slice(&value.to_be_bytes());
        Ok(())
    }

    #[derive(Debug, Clone)]
    pub(crate) struct AvcDecoderConfig {
        pub sps: Vec<u8>,
        pub pps: Vec<u8>,
        pub length_size_minus_one: u8,
    }

    pub(crate) fn parse_avcc(data: &[u8]) -> Result<AvcDecoderConfig> {
        if data.len() < 7 {
            return Err(Error::bitstream("avcC box is too short"));
        }
        let length_size_minus_one = data[4] & 0x03;
        let num_sps = (data[5] & 0x1f) as usize;
        if num_sps == 0 {
            return Err(Error::bitstream("avcC contains no SPS"));
        }
        let mut offset = 6;
        let mut sps = Vec::new();
        for _ in 0..num_sps {
            if offset + 2 > data.len() {
                return Err(Error::bitstream("truncated avcC SPS length"));
            }
            let len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + len > data.len() {
                return Err(Error::bitstream("truncated avcC SPS data"));
            }
            if sps.is_empty() {
                sps = data[offset..offset + len].to_vec();
            }
            offset += len;
        }
        if offset >= data.len() {
            return Err(Error::bitstream("avcC is missing PPS count"));
        }
        let num_pps = data[offset] as usize;
        offset += 1;
        let mut pps = Vec::new();
        for _ in 0..num_pps {
            if offset + 2 > data.len() {
                return Err(Error::bitstream("truncated avcC PPS length"));
            }
            let len = u16::from_be_bytes([data[offset], data[offset + 1]]) as usize;
            offset += 2;
            if offset + len > data.len() {
                return Err(Error::bitstream("truncated avcC PPS data"));
            }
            if pps.is_empty() {
                pps = data[offset..offset + len].to_vec();
            }
            offset += len;
        }
        if sps.is_empty() {
            return Err(Error::bitstream("avcC contains no SPS data"));
        }
        if pps.is_empty() {
            return Err(Error::bitstream("avcC contains no PPS data"));
        }
        Ok(AvcDecoderConfig {
            sps,
            pps,
            length_size_minus_one,
        })
    }

    pub(crate) fn parse_sps(sps: &[u8]) -> Result<SpsInfo> {
        if sps.len() < 2 || (sps[0] & 0x1f) != 7 {
            return Err(Error::bitstream("expected H.264 SPS NAL unit"));
        }

        let rbsp = remove_emulation_prevention(&sps[1..]);
        let mut bits = BitReader::new(&rbsp);
        let profile_idc = bits.read_bits(8)? as u8;
        bits.read_bits(8)?; // constraint flags + reserved
        bits.read_bits(8)?; // level_idc
        bits.read_ue()?; // seq_parameter_set_id

        if matches!(
            profile_idc,
            100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
        ) {
            let chroma_format_idc = bits.read_ue()?;
            if chroma_format_idc == 3 {
                bits.read_bit()?;
            }
            bits.read_ue()?; // bit_depth_luma_minus8
            bits.read_ue()?; // bit_depth_chroma_minus8
            bits.read_bit()?; // qpprime_y_zero_transform_bypass_flag
            if bits.read_bit()? {
                let count = if chroma_format_idc != 3 { 8 } else { 12 };
                for index in 0..count {
                    if bits.read_bit()? {
                        skip_scaling_list(&mut bits, if index < 6 { 16 } else { 64 })?;
                    }
                }
            }
        }

        bits.read_ue()?; // log2_max_frame_num_minus4
        let pic_order_cnt_type = bits.read_ue()?;
        if pic_order_cnt_type == 0 {
            bits.read_ue()?; // log2_max_pic_order_cnt_lsb_minus4
        } else if pic_order_cnt_type == 1 {
            bits.read_bit()?; // delta_pic_order_always_zero_flag
            bits.read_se()?;
            bits.read_se()?;
            let count = bits.read_ue()?;
            for _ in 0..count {
                bits.read_se()?;
            }
        }

        bits.read_ue()?; // max_num_ref_frames
        bits.read_bit()?; // gaps_in_frame_num_value_allowed_flag
        let pic_width_in_mbs_minus1 = bits.read_ue()?;
        let pic_height_in_map_units_minus1 = bits.read_ue()?;
        let frame_mbs_only_flag = bits.read_bit()?;
        if !frame_mbs_only_flag {
            bits.read_bit()?; // mb_adaptive_frame_field_flag
        }
        bits.read_bit()?; // direct_8x8_inference_flag

        let mut crop_left = 0;
        let mut crop_right = 0;
        let mut crop_top = 0;
        let mut crop_bottom = 0;
        if bits.read_bit()? {
            crop_left = bits.read_ue()?;
            crop_right = bits.read_ue()?;
            crop_top = bits.read_ue()?;
            crop_bottom = bits.read_ue()?;
        }

        let width =
            ((pic_width_in_mbs_minus1 + 1) * 16) as i64 - ((crop_left + crop_right) * 2) as i64;
        let frame_height =
            (2 - u32::from(frame_mbs_only_flag)) * (pic_height_in_map_units_minus1 + 1) * 16;
        let crop_unit_y = if frame_mbs_only_flag { 2 } else { 4 };
        let height = frame_height as i64 - ((crop_top + crop_bottom) * crop_unit_y) as i64;
        if width <= 0 || height <= 0 || width > u16::MAX as i64 || height > u16::MAX as i64 {
            return Err(Error::bitstream("invalid dimensions in H.264 SPS"));
        }

        Ok(SpsInfo {
            width: width as u16,
            height: height as u16,
        })
    }

    pub(crate) fn remove_emulation_prevention(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len());
        let mut zero_count = 0;
        for &byte in data {
            if zero_count >= 2 && byte == 0x03 {
                zero_count = 0;
                continue;
            }
            out.push(byte);
            if byte == 0 {
                zero_count += 1;
            } else {
                zero_count = 0;
            }
        }
        out
    }

    fn skip_scaling_list(bits: &mut BitReader<'_>, size: usize) -> Result<()> {
        let mut last_scale = 8_i32;
        let mut next_scale = 8_i32;
        for _ in 0..size {
            if next_scale != 0 {
                let delta_scale = bits.read_se()?;
                next_scale = (last_scale + delta_scale + 256) % 256;
            }
            last_scale = if next_scale == 0 {
                last_scale
            } else {
                next_scale
            };
        }
        Ok(())
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn converts_annex_b_to_length_prefixed() {
            let data = [0, 0, 1, 0x65, 1, 2, 0, 0, 0, 1, 0x41, 3];
            let out = annex_b_to_length_prefixed(&data).unwrap();
            assert_eq!(out, vec![0, 0, 0, 3, 0x65, 1, 2, 0, 0, 0, 2, 0x41, 3]);
            assert!(contains_idr(&data));
        }
    }
}

pub(crate) mod hevc {
    use crate::codecs::avc::remove_emulation_prevention;
    use crate::codecs::bit_reader::BitReader;
    use crate::error::{Error, Result};

    pub(crate) const NAL_VPS: u8 = 32;
    pub(crate) const NAL_SPS: u8 = 33;
    pub(crate) const NAL_PPS: u8 = 34;

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) struct SpsInfo {
        pub width: u16,
        pub height: u16,
        pub general_profile_space: u8,
        pub general_tier_flag: bool,
        pub general_profile_idc: u8,
        pub general_profile_compatibility_flags: u32,
        pub general_constraint_indicator_flags: [u8; 6],
        pub general_level_idc: u8,
        pub chroma_format_idc: u8,
        pub bit_depth_luma_minus8: u8,
        pub bit_depth_chroma_minus8: u8,
        pub temporal_id_nesting_flag: bool,
    }

    #[derive(Debug, Clone)]
    pub(crate) struct HevcDecoderConfig {
        pub vps: Vec<u8>,
        pub sps: Vec<u8>,
        pub pps: Vec<u8>,
        pub length_size_minus_one: u8,
    }

    /// HEVC NAL unit type from a 2-byte NAL header.
    pub(crate) fn nal_type(data: &[u8]) -> Option<u8> {
        if data.is_empty() {
            None
        } else {
            Some((data[0] >> 1) & 0x3f)
        }
    }

    pub(crate) fn extract_vps_sps_pps(
        data: &[u8],
    ) -> (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>) {
        let mut vps = None;
        let mut sps = None;
        let mut pps = None;
        for unit in crate::codecs::avc::nal_units_annex_b(data) {
            if unit.data.is_empty() {
                continue;
            }
            match nal_type(unit.data) {
                Some(NAL_VPS) if vps.is_none() => vps = Some(unit.data.to_vec()),
                Some(NAL_SPS) if sps.is_none() => sps = Some(unit.data.to_vec()),
                Some(NAL_PPS) if pps.is_none() => pps = Some(unit.data.to_vec()),
                _ => {}
            }
        }
        (vps, sps, pps)
    }

    pub(crate) fn contains_irap(data: &[u8]) -> bool {
        crate::codecs::avc::nal_units_annex_b(data)
            .into_iter()
            .any(|unit| matches!(nal_type(unit.data), Some(t) if (16..=23).contains(&t)))
    }

    pub(crate) fn parse_sps(sps: &[u8]) -> Result<SpsInfo> {
        if sps.len() < 3 || nal_type(sps) != Some(NAL_SPS) {
            return Err(Error::bitstream("expected HEVC SPS NAL unit"));
        }

        let rbsp = remove_emulation_prevention(&sps[2..]);
        let mut bits = BitReader::new(&rbsp);

        bits.read_bits(4)?; // sps_video_parameter_set_id
        let max_sub_layers_minus1 = bits.read_bits(3)? as u8;
        if max_sub_layers_minus1 != 0 {
            return Err(Error::unsupported(
                "HEVC SPS with multiple temporal sub-layers is out of Phase 3 scope",
            ));
        }
        let temporal_id_nesting_flag = bits.read_bit()?;

        let general_profile_space = bits.read_bits(2)? as u8;
        let general_tier_flag = bits.read_bit()?;
        let general_profile_idc = bits.read_bits(5)? as u8;
        let general_profile_compatibility_flags = bits.read_bits(32)?;
        let mut constraint = [0u8; 6];
        for byte in constraint.iter_mut() {
            *byte = bits.read_bits(8)? as u8;
        }
        let general_level_idc = bits.read_bits(8)? as u8;
        // maxSubLayersMinus1 == 0: no sub-layer present flags, no reserved bits.

        bits.read_ue()?; // sps_seq_parameter_set_id
        let chroma_format_idc = bits.read_ue()? as u8;
        if chroma_format_idc == 3 {
            bits.read_bit()?; // separate_colour_plane_flag
        }
        let pic_width = bits.read_ue()?;
        let pic_height = bits.read_ue()?;

        let mut crop_left = 0u32;
        let mut crop_right = 0u32;
        let mut crop_top = 0u32;
        let mut crop_bottom = 0u32;
        if bits.read_bit()? {
            crop_left = bits.read_ue()?;
            crop_right = bits.read_ue()?;
            crop_top = bits.read_ue()?;
            crop_bottom = bits.read_ue()?;
        }
        let bit_depth_luma_minus8 = bits.read_ue()? as u8;
        let bit_depth_chroma_minus8 = bits.read_ue()? as u8;

        let (sub_width_c, sub_height_c) = match chroma_format_idc {
            0 => (1u32, 1u32),
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let width =
            pic_width as i64 - (crop_left as i64 + crop_right as i64) * sub_width_c as i64;
        let height =
            pic_height as i64 - (crop_top as i64 + crop_bottom as i64) * sub_height_c as i64;
        if width <= 0 || height <= 0 || width > u16::MAX as i64 || height > u16::MAX as i64 {
            return Err(Error::bitstream("invalid dimensions in HEVC SPS"));
        }

        Ok(SpsInfo {
            width: width as u16,
            height: height as u16,
            general_profile_space,
            general_tier_flag,
            general_profile_idc,
            general_profile_compatibility_flags,
            general_constraint_indicator_flags: constraint,
            general_level_idc,
            chroma_format_idc,
            bit_depth_luma_minus8,
            bit_depth_chroma_minus8,
            temporal_id_nesting_flag,
        })
    }

    pub(crate) fn hvcc(vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<Vec<u8>> {
        let info = parse_sps(sps)?;
        let mut out = Vec::new();
        out.push(1); // configurationVersion
        out.push(
            (info.general_profile_space << 6)
                | (u8::from(info.general_tier_flag) << 5)
                | info.general_profile_idc,
        );
        out.extend_from_slice(&info.general_profile_compatibility_flags.to_be_bytes());
        out.extend_from_slice(&info.general_constraint_indicator_flags);
        out.push(info.general_level_idc);
        out.extend_from_slice(&0xF000u16.to_be_bytes()); // min_spatial_segmentation_idc=0, reserved 0xF
        out.push(0xFC); // parallelismType=0, reserved 0xFC
        out.push(0xFC | (info.chroma_format_idc & 0x03));
        out.push(0xF8 | (info.bit_depth_luma_minus8 & 0x07));
        out.push(0xF8 | (info.bit_depth_chroma_minus8 & 0x07));
        out.extend_from_slice(&0u16.to_be_bytes()); // avgFrameRate=0
        let flags = (1u8 << 3) // numTemporalLayers=1
            | (u8::from(info.temporal_id_nesting_flag) << 2)
            | 3; // lengthSizeMinusOne=3
        out.push(flags);
        out.push(3); // numOfArrays
        push_array(&mut out, NAL_VPS, vps)?;
        push_array(&mut out, NAL_SPS, sps)?;
        push_array(&mut out, NAL_PPS, pps)?;
        Ok(out)
    }

    fn push_array(out: &mut Vec<u8>, nal_type: u8, data: &[u8]) -> Result<()> {
        out.push(nal_type & 0x3f); // array_completeness=0, reserved=0, NAL_unit_type
        out.extend_from_slice(&1u16.to_be_bytes()); // numNalus=1
        let len = u16::try_from(data.len())
            .map_err(|_| Error::bitstream("HEVC decoder config NAL unit is too large"))?;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(data);
        Ok(())
    }

    pub(crate) fn parse_hvcc(data: &[u8]) -> Result<HevcDecoderConfig> {
        if data.len() < 23 {
            return Err(Error::bitstream("hvcC box is too short"));
        }
        let length_size_minus_one = data[21] & 0x03;
        let num_arrays = data[22] as usize;
        let mut vps = None;
        let mut sps = None;
        let mut pps = None;
        let mut pos = 23;
        for _ in 0..num_arrays {
            if pos + 3 > data.len() {
                return Err(Error::bitstream("truncated hvcC array header"));
            }
            let nal_type = data[pos] & 0x3f;
            pos += 1;
            let num_nalus = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
            pos += 2;
            for _ in 0..num_nalus {
                if pos + 2 > data.len() {
                    return Err(Error::bitstream("truncated hvcC NAL unit length"));
                }
                let len = u16::from_be_bytes([data[pos], data[pos + 1]]) as usize;
                pos += 2;
                if pos + len > data.len() {
                    return Err(Error::bitstream("truncated hvcC NAL unit data"));
                }
                let nal = data[pos..pos + len].to_vec();
                pos += len;
                match nal_type {
                    NAL_VPS if vps.is_none() => vps = Some(nal),
                    NAL_SPS if sps.is_none() => sps = Some(nal),
                    NAL_PPS if pps.is_none() => pps = Some(nal),
                    _ => {}
                }
            }
        }
        Ok(HevcDecoderConfig {
            vps: vps.ok_or_else(|| Error::bitstream("hvcC box is missing VPS"))?,
            sps: sps.ok_or_else(|| Error::bitstream("hvcC box is missing SPS"))?,
            pps: pps.ok_or_else(|| Error::bitstream("hvcC box is missing PPS"))?,
            length_size_minus_one,
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        // Minimal HEVC SPS for a 1280x720, 4:2:0, 8-bit, main profile stream.
        // NAL header (2 bytes) + RBSP. Built to exercise the parse path.
        fn sample_sps_rbsp() -> Vec<u8> {
            // We construct the SPS via a bit writer to avoid hand-rolling hex.
            let mut w = BitWriter::new();
            w.u(4, 0); // sps_video_parameter_set_id
            w.u(3, 0); // sps_max_sub_layers_minus1
            w.bit(true); // sps_temporal_id_nesting_flag
            // profile_tier_level (general)
            w.u(2, 0); // general_profile_space
            w.bit(false); // general_tier_flag
            w.u(5, 1); // general_profile_idc (Main)
            w.u(32, 0x60000000); // general_profile_compatibility_flags
            for _ in 0..6 {
                w.u(8, 0); // constraint flags
            }
            w.u(8, 93); // general_level_idc (level 3.1)
            w.ue(0); // sps_seq_parameter_set_id
            w.ue(1); // chroma_format_idc (4:2:0)
            w.ue(1280); // pic_width_in_luma_samples
            w.ue(720); // pic_height_in_luma_samples
            w.bit(false); // conformance_window_flag
            w.ue(0); // bit_depth_luma_minus8
            w.ue(0); // bit_depth_chroma_minus8
            w.to_bytes()
        }

        struct BitWriter {
            bytes: Vec<u8>,
            current: u8,
            pos: u8,
        }

        impl BitWriter {
            fn new() -> Self {
                Self {
                    bytes: Vec::new(),
                    current: 0,
                    pos: 0,
                }
            }
            fn bit(&mut self, set: bool) {
                if set {
                    self.current |= 1 << (7 - self.pos);
                }
                self.pos += 1;
                if self.pos == 8 {
                    self.bytes.push(self.current);
                    self.current = 0;
                    self.pos = 0;
                }
            }
            fn u(&mut self, count: usize, mut value: u32) {
                for i in (0..count).rev() {
                    self.bit((value >> i) & 1 == 1);
                }
                let _ = &mut value;
            }
            fn ue(&mut self, mut value: u32) {
                let v = value + 1;
                let mut zeros = 0;
                while (v >> zeros) > 1 {
                    zeros += 1;
                }
                for _ in 0..zeros {
                    self.bit(false);
                }
                for i in (0..=zeros).rev() {
                    self.bit((v >> i) & 1 == 1);
                }
                let _ = &mut value;
            }
            fn to_bytes(mut self) -> Vec<u8> {
                if self.pos > 0 {
                    self.bytes.push(self.current);
                }
                self.bytes
            }
        }

        fn make_sps_nal(rbsp: &[u8]) -> Vec<u8> {
            let mut sps = vec![0x42, 0x01]; // NAL header: nal_unit_type=33 (SPS)
            sps.extend_from_slice(rbsp);
            sps
        }

        #[test]
        fn parses_hevc_sps_dimensions() {
            let sps = make_sps_nal(&sample_sps_rbsp());
            let info = parse_sps(&sps).unwrap();
            assert_eq!(info.width, 1280);
            assert_eq!(info.height, 720);
            assert_eq!(info.general_profile_idc, 1);
            assert_eq!(info.general_level_idc, 93);
            assert_eq!(info.chroma_format_idc, 1);
        }

        #[test]
        fn hvcc_roundtrip() {
            let sps = make_sps_nal(&sample_sps_rbsp());
            let vps = vec![0x40u8, 0x01, 0x0c]; // NAL type 32 (VPS) placeholder
            let pps = vec![0x44u8, 0x01, 0xc1]; // NAL type 34 (PPS) placeholder
            let encoded = hvcc(&vps, &sps, &pps).unwrap();
            assert_eq!(encoded[0], 1); // configurationVersion
            assert_eq!(encoded[encoded.len() - 1], 0xc1); // last PPS byte
            let config = parse_hvcc(&encoded).unwrap();
            assert_eq!(config.vps, vps);
            assert_eq!(config.sps, sps);
            assert_eq!(config.pps, pps);
            assert_eq!(config.length_size_minus_one, 3);
        }

        #[test]
        fn detects_irap() {
            // NAL header for IDR_W_RADL (type 19): forbidden=0, type=19, layer=0, tid=1
            // byte0 = (0<<7) | (19<<1) | 0 = 0x26, byte1 = (0<<3) | 1 = 0x01
            let idr = [0, 0, 0, 1, 0x26, 0x01, 0xAA];
            assert!(contains_irap(&idr));
            // NAL type 1 (TRAIL_R): byte0 = (1<<1) = 0x02
            let non_irap = [0, 0, 1, 0x02, 0x01, 0x55];
            assert!(!contains_irap(&non_irap));
        }
    }
}
