//! VP8 frame headers (RFC 6386 §9.1), read without libvpx.

/// Whether `data` (one VP8 frame) is a key frame: bit 0 of the frame tag is 0 for key frames.
pub(crate) fn is_keyframe(data: &[u8]) -> bool {
	data.first().is_some_and(|b| b & 1 == 0)
}

/// Picture size declared by a VP8 key frame header, or `None` for interframes, which keep
/// the size of the last key frame.
pub(crate) fn keyframe_size(data: &[u8]) -> Option<(u32, u32)> {
	if data.len() < 10 || !is_keyframe(data) || data[3..6] != [0x9d, 0x01, 0x2a] {
		return None;
	}
	// The top two bits of each are a scaling hint, not part of the size.
	let dim = |i: usize| u32::from(u16::from_le_bytes([data[i], data[i + 1]]) & 0x3fff);
	Some((dim(6), dim(8)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_key_frame_headers() {
		assert!(is_keyframe(&[0x10, 0x02, 0x00]));
		assert!(!is_keyframe(&[0x11, 0x02, 0x00]));
		assert!(!is_keyframe(&[]));

		let header = [0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0xc0, 0x30, 0x00];
		assert_eq!(keyframe_size(&header), Some((64, 48)));
		// Interframes and frames without the start code declare no size.
		assert_eq!(
			keyframe_size(&[0x11, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0x40, 0x00, 0x30, 0x00]),
			None
		);
		assert_eq!(
			keyframe_size(&[0x10, 0x02, 0x00, 0, 0, 0, 0x40, 0x00, 0x30, 0x00]),
			None
		);
		assert_eq!(keyframe_size(&header[..9]), None);
	}
}
