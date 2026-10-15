//! H.264 Annex-B byte streams, read without a decoder.

use hang::catalog::VideoCodec;

/// Whether `data` (one access unit) holds an IDR picture.
pub(crate) fn is_keyframe(data: &[u8]) -> bool {
	nals(data).any(|nal| nal.first().map(|h| h & 0x1f) == Some(5))
}

/// `avc3.PPCCLL` (SPS/PPS inline) for an Annex-B keyframe, read from its SPS.
pub(crate) fn catalog_codec(keyframe: &[u8]) -> VideoCodec {
	let sps = nals(keyframe).find(|nal| nal.first().map(|h| h & 0x1f) == Some(7) && nal.len() >= 4);
	let (profile, constraints, level) = sps.map_or((0x42, 0xc0, 0x1f), |sps| (sps[1], sps[2], sps[3]));
	VideoCodec::H264(hang::catalog::H264 {
		inline: true,
		profile,
		constraints,
		level,
	})
}

/// Iterate the NAL units (without start codes) of an Annex-B byte stream.
pub(crate) fn nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
	let mut starts = Vec::new();
	let mut i = 0;
	while i + 3 <= data.len() {
		if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
			starts.push(i + 3);
			i += 3;
		} else {
			i += 1;
		}
	}
	let ends: Vec<usize> = starts
		.iter()
		.skip(1)
		.map(|&s| {
			// Trim the start code and any zero bytes preceding it (4-byte start codes).
			let mut e = s - 3;
			while e > 0 && data[e - 1] == 0 {
				e -= 1;
			}
			e
		})
		.chain(std::iter::once(data.len()))
		.collect();
	starts.into_iter().zip(ends).map(move |(s, e)| &data[s..e.max(s)])
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn nal_splitter_handles_3_and_4_byte_start_codes() {
		let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
		let nals: Vec<&[u8]> = nals(&data).collect();
		assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
	}

	#[test]
	fn nal_splitter_ignores_leading_bytes_and_streams_without_start_codes() {
		assert_eq!(nals(&[]).count(), 0);
		assert_eq!(nals(&[1, 2, 3, 0, 0]).count(), 0);
		let nals: Vec<&[u8]> = nals(&[9, 9, 0, 0, 1, 0x41, 5]).collect();
		assert_eq!(nals, vec![&[0x41, 5][..]]);
	}
}
