//! Just enough AV1 bitstream parsing (AV1 spec §5) to find keyframes, describe a stream in the
//! catalog, and bound picture sizes before a decoder allocates for them.
//!
//! Frames are temporal units in the low-overhead bitstream format: OBUs that carry their size.

use hang::catalog::{AV1, VideoCodec};

const OBU_SEQUENCE_HEADER: u8 = 1;
const OBU_FRAME_HEADER: u8 = 3;
const OBU_FRAME: u8 = 6;

/// Iterate the `(type, payload)` of each OBU in a temporal unit, stopping at the first malformed one.
fn obus(mut data: &[u8]) -> impl Iterator<Item = (u8, &[u8])> {
	std::iter::from_fn(move || {
		let (obu_type, payload, rest) = split_obu(data)?;
		data = rest;
		Some((obu_type, payload))
	})
}

/// Split the first OBU off `data` into its type, its payload and the bytes after it.
fn split_obu(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
	let (&header, rest) = data.split_first()?;
	let obu_type = (header >> 3) & 0xf;
	let has_extension = header & 0x04 != 0;
	let has_size = header & 0x02 != 0;
	let rest = if has_extension { rest.get(1..)? } else { rest };
	let (payload, rest) = if has_size {
		let (size, used) = leb128(rest)?;
		let rest = &rest[used..];
		let size = usize::try_from(size).ok().filter(|s| *s <= rest.len())?;
		rest.split_at(size)
	} else {
		(rest, &[][..])
	};
	Some((obu_type, payload, rest))
}

/// Decode an unsigned LEB128 value (at most 8 bytes), returning it and the bytes it used.
fn leb128(data: &[u8]) -> Option<(u64, usize)> {
	let mut value = 0u64;
	for (i, byte) in data.iter().take(8).enumerate() {
		value |= u64::from(byte & 0x7f) << (i * 7);
		if byte & 0x80 == 0 {
			return Some((value, i + 1));
		}
	}
	None
}

/// The sequence header of a temporal unit, if it has one.
pub(crate) fn sequence_header(data: &[u8]) -> Option<SequenceHeader> {
	obus(data)
		.find(|(t, _)| *t == OBU_SEQUENCE_HEADER)
		.and_then(|(_, payload)| SequenceHeader::parse(payload))
}

/// Every sequence header in a temporal unit, in order, with `None` for one that doesn't parse.
///
/// A decoder applies each sequence header it meets, so bounding the picture size it allocates
/// for takes all of them, not just the first. libaom also carries on past zero bytes between
/// OBUs (it allows them after each frame), so these are skipped rather than ending the search.
#[cfg_attr(not(feature = "av1"), allow(dead_code))]
pub(crate) fn sequence_headers(mut data: &[u8]) -> impl Iterator<Item = Option<SequenceHeader>> {
	std::iter::from_fn(move || {
		loop {
			let padding = data.iter().take_while(|&&b| b == 0).count();
			let (obu_type, payload, rest) = split_obu(&data[padding..])?;
			data = rest;
			if obu_type == OBU_SEQUENCE_HEADER {
				return Some(SequenceHeader::parse(payload));
			}
		}
	})
}

/// Catalog description (`av01.P.LLT.DD`) of a keyframe, read from its sequence header, falling
/// back to Main profile, level 4.0, 8-bit.
#[cfg_attr(not(any(feature = "av1", feature = "hw")), allow(dead_code))]
pub(crate) fn catalog_codec(keyframe: &[u8]) -> VideoCodec {
	sequence_header(keyframe).map_or_else(
		|| {
			VideoCodec::AV1(AV1 {
				level: 8,
				..Default::default()
			})
		},
		|header| header.catalog_codec(),
	)
}

/// Whether a temporal unit starts a coded video sequence: it carries a sequence header, and
/// its first frame is a shown key frame, so a decoder can start from it.
pub(crate) fn is_keyframe(data: &[u8]) -> bool {
	let Some(sequence) = sequence_header(data) else {
		return false;
	};
	let Some((_, frame)) = obus(data).find(|(t, _)| matches!(*t, OBU_FRAME_HEADER | OBU_FRAME)) else {
		return false;
	};
	if sequence.reduced_still_picture_header {
		return true;
	}
	// show_existing_frame (1 bit) = 0, then frame_type (2 bits) = KEY_FRAME (0).
	frame.first().is_some_and(|b| b & 0xe0 == 0)
}

/// Fields of an AV1 sequence header (§5.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SequenceHeader {
	pub profile: u8,
	pub reduced_still_picture_header: bool,
	/// `seq_level_idx` of operating point 0.
	pub level: u8,
	/// `seq_tier` of operating point 0 (true for the High tier).
	pub high_tier: bool,
	pub max_width: u32,
	pub max_height: u32,
	pub bit_depth: u8,
	pub mono_chrome: bool,
}

impl SequenceHeader {
	pub fn parse(payload: &[u8]) -> Option<Self> {
		let mut r = BitReader::new(payload);
		let profile = r.byte(3)?;
		let _still_picture = r.flag()?;
		let reduced = r.flag()?;

		let (level, high_tier);
		if reduced {
			level = r.byte(5)?;
			high_tier = false;
		} else {
			let mut buffer_delay_bits = 0;
			let mut decoder_model_info = false;
			if r.flag()? {
				// timing_info()
				r.skip(64)?;
				if r.flag()? {
					r.uvlc()?;
				}
				decoder_model_info = r.flag()?;
				if decoder_model_info {
					buffer_delay_bits = r.bits(5)? + 1;
					r.skip(32 + 5 + 5)?;
				}
			}
			let initial_display_delay = r.flag()?;
			let operating_points = r.bits(5)? + 1;
			let mut first = None;
			for _ in 0..operating_points {
				let _idc = r.bits(12)?;
				let level = r.byte(5)?;
				let tier = level > 7 && r.flag()?;
				first.get_or_insert((level, tier));
				if decoder_model_info && r.flag()? {
					r.skip(2 * buffer_delay_bits + 1)?;
				}
				if initial_display_delay && r.flag()? {
					r.skip(4)?;
				}
			}
			(level, high_tier) = first?;
		}

		let width_bits = r.bits(4)? + 1;
		let height_bits = r.bits(4)? + 1;
		let max_width = r.bits(width_bits)? + 1;
		let max_height = r.bits(height_bits)? + 1;

		if !reduced && r.flag()? {
			// delta_frame_id_length_minus_2, additional_frame_id_length_minus_1
			r.skip(4 + 3)?;
		}
		// use_128x128_superblock, enable_filter_intra, enable_intra_edge_filter
		r.skip(3)?;
		if !reduced {
			// enable_interintra_compound, enable_masked_compound, enable_warped_motion, enable_dual_filter
			r.skip(4)?;
			let order_hint = r.flag()?;
			if order_hint {
				// enable_jnt_comp, enable_ref_frame_mvs
				r.skip(2)?;
			}
			let force_screen_content_tools = if r.flag()? { 2 } else { r.bits(1)? };
			if force_screen_content_tools > 0 && !r.flag()? {
				// seq_force_integer_mv
				r.skip(1)?;
			}
			if order_hint {
				r.skip(3)?;
			}
		}
		// enable_superres, enable_cdef, enable_restoration
		r.skip(3)?;

		// color_config()
		let high_bitdepth = r.flag()?;
		let bit_depth = match (profile, high_bitdepth) {
			(2, true) if r.flag()? => 12,
			(_, true) => 10,
			_ => 8,
		};
		let mono_chrome = profile != 1 && r.flag()?;

		Some(Self {
			profile,
			reduced_still_picture_header: reduced,
			level,
			high_tier,
			max_width,
			max_height,
			bit_depth,
			mono_chrome,
		})
	}

	/// Catalog description, e.g. `av01.0.08M.08`.
	///
	/// An encoder may leave the level unconstrained (31), which browsers may not accept, so then
	/// the level is the lowest one whose picture size limit (Annex A.3) fits the stream.
	#[cfg_attr(not(any(feature = "av1", feature = "hw")), allow(dead_code))]
	pub fn catalog_codec(&self) -> VideoCodec {
		const UNCONSTRAINED: u8 = 31;
		// (seq_level_idx, MaxPicSize): levels 2.0, 2.1, 3.0, 3.1, 4.0, 5.0, 6.0.
		const LEVELS: [(u8, u64); 7] = [
			(0, 147_456),
			(1, 278_784),
			(4, 665_856),
			(5, 1_065_024),
			(8, 2_359_296),
			(12, 8_912_896),
			(16, 35_651_584),
		];
		let level = if self.level == UNCONSTRAINED {
			let pixels = u64::from(self.max_width) * u64::from(self.max_height);
			LEVELS
				.iter()
				.find(|(_, max)| pixels <= *max)
				.map_or(16, |(level, _)| *level)
		} else {
			self.level
		};
		VideoCodec::AV1(AV1 {
			profile: self.profile,
			level,
			// Only levels above 3.3 (7) have a High tier.
			tier: if self.high_tier && level > 7 { 'H' } else { 'M' },
			bitdepth: self.bit_depth,
			mono_chrome: self.mono_chrome,
			..Default::default()
		})
	}
}

/// MSB-first bit reader.
struct BitReader<'a> {
	data: &'a [u8],
	pos: usize,
}

impl<'a> BitReader<'a> {
	fn new(data: &'a [u8]) -> Self {
		Self { data, pos: 0 }
	}

	fn flag(&mut self) -> Option<bool> {
		let byte = self.data.get(self.pos / 8)?;
		let bit = byte >> (7 - self.pos % 8) & 1;
		self.pos += 1;
		Some(bit == 1)
	}

	/// Read `n` (at most 32) bits.
	fn bits(&mut self, n: u32) -> Option<u32> {
		(0..n).try_fold(0u32, |acc, _| Some(acc << 1 | u32::from(self.flag()?)))
	}

	/// Read `n` (at most 8) bits.
	fn byte(&mut self, n: u32) -> Option<u8> {
		u8::try_from(self.bits(n)?).ok()
	}

	fn skip(&mut self, n: u32) -> Option<()> {
		self.pos += n as usize;
		(self.pos <= self.data.len() * 8).then_some(())
	}

	/// Variable-length unsigned value (§4.10.3).
	fn uvlc(&mut self) -> Option<u32> {
		let mut leading_zeros = 0;
		while !self.flag()? {
			leading_zeros += 1;
		}
		if leading_zeros >= 32 {
			return Some(u32::MAX);
		}
		Some(self.bits(leading_zeros)? + ((1u32 << leading_zeros) - 1))
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;

	/// MSB-first bit writer for building test bitstreams.
	#[derive(Default)]
	pub(crate) struct BitWriter {
		bytes: Vec<u8>,
		bits: usize,
	}

	impl BitWriter {
		pub fn put(&mut self, n: u32, value: u32) -> &mut Self {
			for i in (0..n).rev() {
				if self.bits.is_multiple_of(8) {
					self.bytes.push(0);
				}
				let bit = (value >> i & 1) as u8;
				*self.bytes.last_mut().unwrap() |= bit << (7 - self.bits % 8);
				self.bits += 1;
			}
			self
		}

		pub fn finish(&self) -> Vec<u8> {
			self.bytes.clone()
		}
	}

	/// Wrap a payload in an OBU with a size field.
	pub(crate) fn obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
		assert!(payload.len() < 128);
		let mut out = vec![obu_type << 3 | 0x02, u8::try_from(payload.len()).unwrap()];
		out.extend_from_slice(payload);
		out
	}

	/// A minimal (not reduced) 8-bit 4:2:0 sequence header with one operating point.
	pub(crate) fn sequence_header_payload(profile: u32, level: u32, width: u32, height: u32) -> Vec<u8> {
		let mut w = BitWriter::default();
		w.put(3, profile).put(1, 0).put(1, 0); // still_picture, reduced_still_picture_header
		w.put(1, 0).put(1, 0); // timing_info_present_flag, initial_display_delay_present_flag
		w.put(5, 0).put(12, 0).put(5, level); // one operating point
		if level > 7 {
			w.put(1, 1); // seq_tier: High
		}
		w.put(4, 15).put(4, 15).put(16, width - 1).put(16, height - 1);
		w.put(1, 0); // frame_id_numbers_present_flag
		w.put(3, 0).put(4, 0); // superblock/intra tools, compound/warp/dual filter
		w.put(1, 1).put(2, 0); // enable_order_hint, jnt_comp, ref_frame_mvs
		w.put(1, 1).put(1, 0); // seq_choose_screen_content_tools, seq_choose_integer_mv
		w.put(3, 6); // order_hint_bits_minus_1
		w.put(3, 0); // superres, cdef, restoration
		w.put(1, 0); // high_bitdepth
		if profile != 1 {
			w.put(1, 0); // mono_chrome
		}
		w.put(1, 0); // color_description_present_flag
		w.finish()
	}

	fn temporal_unit(frame_type: u8, with_sequence_header: bool) -> Vec<u8> {
		let mut tu = obu(2, &[]);
		if with_sequence_header {
			tu.extend(obu(OBU_SEQUENCE_HEADER, &sequence_header_payload(0, 8, 640, 480)));
		}
		tu.extend(obu(OBU_FRAME, &[frame_type << 5 | 0x10, 0xaa, 0xbb]));
		tu
	}

	#[test]
	fn parses_sequence_header() {
		let header = SequenceHeader::parse(&sequence_header_payload(0, 5, 640, 480)).unwrap();
		assert_eq!((header.profile, header.level, header.high_tier), (0, 5, false));
		assert_eq!((header.max_width, header.max_height, header.bit_depth), (640, 480, 8));
		assert!(!header.mono_chrome && !header.reduced_still_picture_header);
		assert_eq!(header.catalog_codec().to_string(), "av01.0.05M.08");
		assert_eq!(catalog_codec(&[]).to_string(), "av01.0.08M.08");
	}

	#[test]
	fn reads_the_tier_above_level_7() {
		let header = SequenceHeader::parse(&sequence_header_payload(0, 12, 3840, 2160)).unwrap();
		assert_eq!((header.level, header.high_tier), (12, true));
		assert_eq!(header.catalog_codec().to_string(), "av01.0.12H.08");
	}

	#[test]
	fn unconstrained_level_follows_the_picture_size() {
		let level = |width, height| {
			SequenceHeader::parse(&sequence_header_payload(0, 31, width, height))
				.unwrap()
				.catalog_codec()
				.to_string()
		};
		assert_eq!(level(320, 240), "av01.0.00M.08");
		assert_eq!(level(1280, 720), "av01.0.05M.08");
		assert_eq!(level(1920, 1080), "av01.0.08H.08");
		assert_eq!(level(16384, 16384), "av01.0.16H.08");
	}

	#[test]
	fn truncated_sequence_headers_fail() {
		let payload = sequence_header_payload(0, 8, 640, 480);
		for len in 0..payload.len() - 1 {
			assert_eq!(SequenceHeader::parse(&payload[..len]), None, "{len} bytes");
		}
	}

	#[test]
	fn finds_every_sequence_header() {
		let header = |width, height| SequenceHeader::parse(&sequence_header_payload(0, 8, width, height));
		let mut data = temporal_unit(0, true);
		// Zero padding after a frame, then a second sequence header and an unreadable one.
		data.extend([0, 0]);
		data.extend(obu(OBU_SEQUENCE_HEADER, &sequence_header_payload(0, 8, 16384, 16384)));
		data.extend(obu(OBU_SEQUENCE_HEADER, &[0x00]));
		let headers: Vec<_> = sequence_headers(&data).collect();
		assert_eq!(headers, vec![header(640, 480), header(16384, 16384), None]);
		assert_eq!(sequence_headers(&temporal_unit(0, false)).count(), 0);
	}

	#[test]
	fn keyframes_need_a_sequence_header_and_a_key_frame() {
		assert!(is_keyframe(&temporal_unit(0, true)));
		assert!(!is_keyframe(&temporal_unit(0, false)));
		// INTER_FRAME and INTRA_ONLY_FRAME don't reset the decoder.
		assert!(!is_keyframe(&temporal_unit(1, true)));
		assert!(!is_keyframe(&temporal_unit(2, true)));
		assert!(!is_keyframe(&[]));
		assert!(!is_keyframe(&[0xff; 16]));
	}

	#[test]
	fn obu_iterator_handles_extensions_and_bad_sizes() {
		// Temporal delimiter with an extension byte, then a padding OBU.
		let data = [0x16, 0x00, 0x00, 0x7a, 0x01, 0x55];
		let parsed: Vec<_> = obus(&data).collect();
		assert_eq!(parsed, vec![(2, &[][..]), (15, &[0x55][..])]);
		// A size running past the end stops iteration.
		assert_eq!(obus(&[0x7a, 0x05, 0x55]).count(), 0);
		// A multi-byte LEB128 size.
		assert_eq!(leb128(&[0x80, 0x01]), Some((128, 2)));
		assert_eq!(leb128(&[0x80; 9]), None);
	}

	#[test]
	fn uvlc_round_trips() {
		let mut w = BitWriter::default();
		// 5 = 0b110 encoded as 2 leading zeros, a one, then 0b11 - (2^2 - 1) = 2 in 2 bits.
		w.put(3, 0b001).put(2, 2);
		let bytes = w.finish();
		assert_eq!(BitReader::new(&bytes).uvlc(), Some(5));
	}
}
