//! Checks shared by every codec backend, and tests of the codec-independent parts.

use super::*;

/// Define a `#[test]` for each shared check of a codec backend, against `$codec`. With
/// `if $guard`, a check is skipped unless `$guard` holds (e.g. the codec's library is present).
macro_rules! shared_codec_tests {
	($codec:expr) => {
		$crate::codec::tests::shared_codec_tests!(@each $codec, {});
	};
	($codec:expr, if $guard:expr) => {
		$crate::codec::tests::shared_codec_tests!(@each $codec, {
			if !$guard {
				return;
			}
		});
	};
	(@each $codec:expr, $skip:tt) => {
		$crate::codec::tests::shared_codec_tests!(
			@define $codec, $skip,
			round_trip,
			keyframes,
			bitrate_changes,
			pixel_formats,
			irregular_timestamps,
			forced_keyframe,
			rejects_invalid_images,
			rejects_oversized_pictures,
			keyframe_after_clock_jump,
		);
	};
	(@define $codec:expr, $skip:tt, $($check:ident),+ $(,)?) => {
		$(
			#[test]
			fn $check() {
				$skip
				$crate::codec::tests::$check($codec);
			}
		)+
	};
}

pub(crate) use shared_codec_tests;

#[expect(clippy::cast_possible_truncation, reason = "the pattern wraps around")]
pub(crate) fn test_pattern(w: u32, h: u32, t: u32) -> Image {
	let rgb: Vec<u8> = (0..h)
		.flat_map(|y| (0..w).flat_map(move |x| [((x + t * 4) % 256) as u8, (y % 256) as u8, 100]))
		.collect();
	Image::new(w, h, PixelFormat::Rgb8, rgb, Duration::from_millis(u64::from(t) * 33)).unwrap()
}

/// Shared checks run against every available codec.
pub(crate) fn round_trip(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		bitrate: 4_000_000,
		..Default::default()
	})
	.unwrap();
	let mut decoder = Decoder::new(codec).unwrap();

	let mut decoded = 0;
	let mut keyframe_codec = None;
	for t in 0..10 {
		let image = test_pattern(64, 48, t);
		let Some(frame) = encoder.encode(&image).unwrap() else {
			continue;
		};
		assert_eq!(Codec::from_catalog(&frame.codec), Some(codec));
		// Delta frames carry the description of the keyframe they depend on.
		let keyframe_codec = keyframe_codec.get_or_insert_with(|| frame.codec.clone());
		assert_eq!(&frame.codec, keyframe_codec);
		assert_eq!(codec.is_keyframe(&frame.data), frame.keyframe);
		if t == 0 {
			assert!(frame.keyframe, "first frame must be a keyframe");
		}
		if let Some(out) = decoder.decode(&frame.data, frame.timestamp).unwrap() {
			assert_eq!((out.width, out.height), (64, 48));
			assert_eq!(out.timestamp, image.timestamp);
			let err =
				out.data
					.iter()
					.zip(image.data.iter())
					.map(|(a, b)| u64::from(a.abs_diff(*b)))
					.sum::<u64>() / out.data.len() as u64;
			assert!(err < 12, "{codec}: mean abs error {err}");
			decoded += 1;
		}
	}
	assert!(decoded >= 9, "{codec}: decoded {decoded} frames");
}

pub(crate) fn keyframes(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		keyframe_interval: Duration::from_millis(100),
		..Default::default()
	})
	.unwrap();
	let keyframes: Vec<bool> = (0..8)
		.filter_map(|t| encoder.encode(&test_pattern(32, 32, t)).unwrap())
		.map(|f| f.keyframe)
		.collect();
	assert!(keyframes[0]);
	assert!(keyframes.iter().filter(|k| **k).count() >= 2, "{keyframes:?}");

	let frame = encoder.encode(&test_pattern(48, 32, 9)).unwrap().unwrap();
	assert!(frame.keyframe, "resolution change restarts with a keyframe");
	assert_eq!((frame.width, frame.height), (48, 32));
}

/// Every input pixel format encodes; odd sizes are cropped to even ones.
pub(crate) fn pixel_formats(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		..Default::default()
	})
	.unwrap();
	let mut decoder = Decoder::new(codec).unwrap();
	for (t, format) in [
		PixelFormat::Rgb8,
		PixelFormat::Bgr8,
		PixelFormat::Rgba8,
		PixelFormat::Bgra8,
		PixelFormat::Mono8,
		PixelFormat::Uyvy,
		PixelFormat::Yuyv,
	]
	.into_iter()
	.enumerate()
	{
		let (width, height) = (33u32, 25u32);
		let data = vec![128u8; (width * height) as usize * format.bytes_per_pixel()];
		let timestamp = Duration::from_millis(t as u64 * 33);
		let image = Image::new(width, height, format, data, timestamp).unwrap();
		encoder.force_keyframe();
		let frame = encoder
			.encode(&image)
			.unwrap()
			.expect("forced keyframes aren't dropped");
		assert!(frame.keyframe, "{codec} {format}");
		assert_eq!((frame.width, frame.height), (32, 24), "{codec} {format}");
		let out = decoder
			.decode(&frame.data, timestamp)
			.unwrap()
			.expect("keyframe decodes");
		assert_eq!((out.width, out.height, out.format), (32, 24, PixelFormat::Rgb8));
	}
}

/// Repeated or backwards capture times still encode, and keep their timestamps.
pub(crate) fn irregular_timestamps(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		..Default::default()
	})
	.unwrap();
	for millis in [1000, 1000, 500, 1033] {
		let mut image = test_pattern(32, 32, 0);
		image.timestamp = Duration::from_millis(millis);
		if let Some(frame) = encoder.encode(&image).unwrap() {
			assert_eq!(frame.timestamp, image.timestamp);
		}
	}
}

pub(crate) fn forced_keyframe(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		keyframe_interval: Duration::from_secs(60),
		..Default::default()
	})
	.unwrap();
	// Rate control may drop unforced frames, but never turns them into keyframes.
	for t in 0..4 {
		if t == 2 {
			encoder.force_keyframe();
		}
		let frame = encoder.encode(&test_pattern(32, 32, t)).unwrap();
		match t {
			0 | 2 => assert!(frame.expect("forced keyframes aren't dropped").keyframe, "frame {t}"),
			_ => assert!(frame.is_none_or(|frame| !frame.keyframe), "frame {t}"),
		}
	}
}

/// A rejected image leaves a running encoder untouched.
pub(crate) fn rejects_invalid_images(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		keyframe_interval: Duration::from_secs(60),
		..Default::default()
	})
	.unwrap();
	assert!(encoder.encode(&test_pattern(32, 32, 0)).unwrap().unwrap().keyframe);
	let tiny = Image::new(1, 1, PixelFormat::Rgb8, vec![0u8; 3], Duration::from_millis(33)).unwrap();
	assert!(matches!(encoder.encode(&tiny), Err(Error::InvalidImage(_))));
	// Same resolution as before, so the encoder carries on without a new keyframe.
	if let Some(frame) = encoder.encode(&test_pattern(32, 32, 2)).unwrap() {
		assert!(!frame.keyframe);
		assert_eq!((frame.width, frame.height), (32, 32));
	}
}

/// A noisy picture, so the encoded size follows the bitrate.
#[expect(clippy::cast_possible_truncation, reason = "the noise wraps around")]
fn noise(w: u32, h: u32, t: u32) -> Image {
	let mut state = 0x9e37_79b9_u32 ^ t.wrapping_mul(0x85eb_ca6b);
	let rgb: Vec<u8> = (0..w * h * 3)
		.map(|_| {
			state ^= state << 13;
			state ^= state >> 17;
			state ^= state << 5;
			state as u8
		})
		.collect();
	Image::new(w, h, PixelFormat::Rgb8, rgb, Duration::from_millis(u64::from(t) * 33)).unwrap()
}

/// Lowering the bitrate mid-stream shrinks the frames that follow.
pub(crate) fn bitrate_changes(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		bitrate: 2_000_000,
		keyframe_interval: Duration::from_secs(60),
		hardware: Hardware::Off,
		..Default::default()
	})
	.unwrap();
	let size = |encoder: &mut Encoder, frames: std::ops::Range<u32>| -> usize {
		frames
			.filter_map(|t| encoder.encode(&noise(128, 96, t)).unwrap())
			.filter(|frame| !frame.keyframe)
			.map(|frame| frame.data.len())
			.sum()
	};
	let high = size(&mut encoder, 0..30);
	encoder.set_bitrate(200_000);
	assert_eq!(encoder.settings().bitrate, 200_000);
	// Let rate control settle before measuring.
	size(&mut encoder, 30..40);
	let low = size(&mut encoder, 40..70);
	assert!(low * 2 < high, "{codec}: {high} bytes at 2 Mbit/s, {low} at 200 kbit/s");
}

/// Pictures above the decoder's limit fail without being converted.
pub(crate) fn rejects_oversized_pictures(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		..Default::default()
	})
	.unwrap();
	let frame = encoder.encode(&test_pattern(64, 48, 0)).unwrap().unwrap();

	let mut decoder = Decoder::new(codec).unwrap();
	assert_eq!(decoder.max_pixels(), DEFAULT_MAX_PIXELS);
	decoder.set_max_pixels(64 * 48 - 1);
	assert!(matches!(
		decoder.decode(&frame.data, frame.timestamp),
		Err(Error::TooLarge {
			width: 64,
			height: 48,
			..
		})
	));

	let mut decoder = Decoder::new(codec).unwrap();
	decoder.set_max_pixels(64 * 48);
	assert!(decoder.decode(&frame.data, frame.timestamp).unwrap().is_some());
}

/// Keyframes keep their interval when the capture clock jumps backwards (e.g. a
/// looping rosbag) or jitters.
pub(crate) fn keyframe_after_clock_jump(codec: Codec) {
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		keyframe_interval: Duration::from_secs(1),
		..Default::default()
	})
	.unwrap();
	let mut encode = |millis: u64| {
		let mut image = test_pattern(32, 32, 0);
		image.timestamp = Duration::from_millis(millis);
		encoder.encode(&image).unwrap()
	};
	assert!(encode(100_000).expect("first frame is a keyframe").keyframe);
	// Neither a small step back nor a restarted clock is due for a keyframe yet.
	for millis in [100_033, 100_032, 0, 500] {
		assert!(encode(millis).is_none_or(|frame| !frame.keyframe), "{millis}ms");
	}
	let frame = encode(1_000).expect("forced keyframes aren't dropped");
	assert!(frame.keyframe, "a second passed since the first keyframe");
}

/// Decoders produce every packed format, and refuse the 4:2:2 ones.
#[test]
fn decodes_into_each_output_format() {
	let Some(&codec) = Codec::available().first() else {
		return;
	};
	if codec == Codec::H264 && std::env::var_os("OPENH264_LIBRARY").is_none() {
		return;
	}
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		hardware: Hardware::Off,
		..Default::default()
	})
	.unwrap();
	let frame = encoder.encode(&test_pattern(64, 48, 0)).unwrap().unwrap();
	let rgb = Decoder::new(codec)
		.unwrap()
		.decode(&frame.data, frame.timestamp)
		.unwrap()
		.unwrap();
	for format in [
		PixelFormat::Bgr8,
		PixelFormat::Rgba8,
		PixelFormat::Bgra8,
		PixelFormat::Mono8,
	] {
		let mut decoder = Decoder::new(codec).unwrap();
		decoder.set_output_format(format).unwrap();
		assert_eq!(decoder.output_format(), format);
		let image = decoder.decode(&frame.data, frame.timestamp).unwrap().unwrap();
		assert_eq!((image.width, image.height, image.format), (64, 48, format));
		if format == PixelFormat::Bgr8 {
			assert_eq!((image.data[0], image.data[2]), (rgb.data[2], rgb.data[0]));
		}
	}
	let mut decoder = Decoder::new(codec).unwrap();
	assert!(matches!(
		decoder.set_output_format(PixelFormat::Uyvy),
		Err(Error::InvalidSettings(_))
	));
	assert_eq!(decoder.output_format(), PixelFormat::Rgb8);
}

#[test]
fn codec_names() {
	assert_eq!("vp8".parse::<Codec>().unwrap(), Codec::Vp8);
	assert_eq!("H264".parse::<Codec>().unwrap(), Codec::H264);
	assert_eq!("AV1".parse::<Codec>().unwrap(), Codec::Av1);
	assert!("vp9".parse::<Codec>().is_err());
	assert!(Codec::Vp8.is_keyframe(&[0x10, 0x02, 0x00]));
	assert!(!Codec::Vp8.is_keyframe(&[0x11, 0x02, 0x00]));
}

#[test]
fn codec_names_round_trip() {
	for &codec in Codec::ALL {
		assert_eq!(codec.to_string().parse::<Codec>().unwrap(), codec);
	}
	assert_eq!("AVC".parse::<Codec>().unwrap(), Codec::H264);
	assert_eq!("av01".parse::<Codec>().unwrap(), Codec::Av1);
	assert!(matches!("".parse::<Codec>(), Err(Error::InvalidSettings(_))));
}

#[test]
fn codecs_from_catalog() {
	assert_eq!(Codec::from_catalog(&VideoCodec::VP8), Some(Codec::Vp8));
	let avc: VideoCodec = "avc3.42001f".parse().unwrap();
	assert_eq!(Codec::from_catalog(&avc), Some(Codec::H264));
	let av1: VideoCodec = "av01.0.04M.08".parse().unwrap();
	assert_eq!(Codec::from_catalog(&av1), Some(Codec::Av1));
	assert_eq!(Codec::from_catalog(&VideoCodec::Unknown("hev2".into())), None);
}

#[test]
fn detects_h264_keyframes_by_idr_nal() {
	let sps_pps_idr = [0, 0, 0, 1, 0x67, 1, 0, 0, 1, 0x68, 2, 0, 0, 1, 0x65, 3];
	assert!(Codec::H264.is_keyframe(&sps_pps_idr));
	assert!(!Codec::H264.is_keyframe(&[0, 0, 0, 1, 0x41, 3]));
	assert!(!Codec::H264.is_keyframe(&[0, 0, 0, 1, 0x67, 1]));
	assert!(!Codec::H264.is_keyframe(&[]));
	assert!(!Codec::Vp8.is_keyframe(&[]));
}

#[test]
fn default_codec_is_compiled_in() {
	if !Codec::available().is_empty() {
		assert!(Codec::default().is_available());
		assert_eq!(Codec::default(), Codec::available()[0]);
	}
}

#[test]
fn missing_codecs_are_reported() {
	for &codec in Codec::ALL {
		if !codec.is_available() {
			let settings = EncoderSettings {
				codec,
				..Default::default()
			};
			assert!(matches!(Encoder::new(settings), Err(Error::CodecUnavailable(_))));
			assert!(matches!(Decoder::new(codec), Err(Error::CodecUnavailable(_))));
		}
	}
}

#[test]
fn hardware_modes() {
	for mode in [Hardware::Off, Hardware::Auto, Hardware::Require] {
		assert_eq!(mode.to_string().parse::<Hardware>().unwrap(), mode);
	}
	assert_eq!("AUTO".parse::<Hardware>().unwrap(), Hardware::Auto);
	assert!(matches!("on".parse::<Hardware>(), Err(Error::InvalidSettings(_))));
	assert_eq!(EncoderSettings::default().hardware, Hardware::Auto);
}

#[test]
fn hardware_off_encodes_in_software() {
	let Some(&codec) = Codec::available().first() else {
		return;
	};
	let mut encoder = Encoder::new(EncoderSettings {
		codec,
		hardware: Hardware::Off,
		..Default::default()
	})
	.unwrap();
	if codec == Codec::H264 && std::env::var_os("OPENH264_LIBRARY").is_none() {
		return;
	}
	assert!(encoder.encode(&test_pattern(32, 32, 0)).unwrap().unwrap().keyframe);
	assert_eq!(encoder.hardware_encoder(), None);
}

#[cfg(not(feature = "hw"))]
#[test]
fn required_hardware_needs_the_hw_feature() {
	let settings = EncoderSettings {
		hardware: Hardware::Require,
		..Default::default()
	};
	if settings.codec.is_available() {
		assert!(matches!(Encoder::new(settings), Err(Error::CodecUnavailable(_))));
	}
}

#[test]
fn encoder_settings_derived_values() {
	let settings = EncoderSettings::default();
	assert_eq!(settings.gop_frames(), 120);
	assert!((1..=4).contains(&settings.threads()));

	let settings = EncoderSettings {
		max_fps: 29.97,
		keyframe_interval: Duration::from_secs(1),
		threads: 8,
		..Default::default()
	};
	assert_eq!(settings.gop_frames(), 60);
	assert_eq!(settings.threads(), 8);

	let settings = EncoderSettings {
		keyframe_interval: Duration::ZERO,
		..Default::default()
	};
	assert_eq!(settings.gop_frames(), 1);
}
