//! Video encoding/decoding.
//!
//! - **VP8** (feature `vp8`, default): royalty-free, via the system libvpx.
//! - **H.264** (feature `h264`): via Cisco's prebuilt OpenH264 binary, loaded at
//!   runtime so Cisco's patent license applies. See [`set_openh264_library`].

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use bytes::Bytes;
use hang::catalog::VideoCodec;

use crate::clock::StreamClock;
use crate::convert::I420;
use crate::{Error, Image, PixelFormat, Result};

#[cfg(feature = "h264")]
mod h264;
#[cfg(feature = "vp8")]
mod vp8;

#[cfg(feature = "h264")]
pub use h264::set_openh264_library;

/// Video codecs moqros can produce and consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Codec {
	Vp8,
	H264,
}

impl Codec {
	/// Codecs compiled into this build, preferred first.
	pub const fn available() -> &'static [Codec] {
		&[
			#[cfg(feature = "vp8")]
			Codec::Vp8,
			#[cfg(feature = "h264")]
			Codec::H264,
		]
	}

	/// Whether this build can encode and decode the codec.
	pub fn is_available(self) -> bool {
		Self::available().contains(&self)
	}

	/// Map a catalog codec onto a moqros codec.
	pub fn from_catalog(codec: &VideoCodec) -> Option<Self> {
		match codec {
			VideoCodec::VP8 => Some(Self::Vp8),
			VideoCodec::H264(_) => Some(Self::H264),
			_ => None,
		}
	}

	/// Whether `data` (one encoded frame) is a keyframe.
	pub fn is_keyframe(self, data: &[u8]) -> bool {
		match self {
			// RFC 6386 §9.1: bit 0 of the frame tag is 0 for key frames.
			Self::Vp8 => data.first().is_some_and(|b| b & 1 == 0),
			Self::H264 => annexb_nals(data).any(|nal| nal.first().map(|h| h & 0x1f) == Some(5)),
		}
	}
}

impl Default for Codec {
	fn default() -> Self {
		Self::available().first().copied().unwrap_or(Self::Vp8)
	}
}

impl fmt::Display for Codec {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Vp8 => "vp8",
			Self::H264 => "h264",
		})
	}
}

impl FromStr for Codec {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self> {
		match s.to_ascii_lowercase().as_str() {
			"vp8" => Ok(Self::Vp8),
			"h264" | "avc" => Ok(Self::H264),
			other => Err(Error::CodecUnavailable(format!("unknown codec {other:?}"))),
		}
	}
}

/// Tuning knobs for [`Encoder`].
#[derive(Debug, Clone)]
pub struct EncoderSettings {
	pub codec: Codec,
	/// Target bitrate in bits per second.
	pub bitrate: u32,
	/// Upper bound on the input frame rate, used by rate control.
	pub max_fps: f32,
	/// Maximum time between keyframes. Each keyframe starts a new MoQ group,
	/// so this bounds how long a late joiner waits for the first picture.
	pub keyframe_interval: Duration,
	/// Encoder worker threads; 0 picks a sensible default.
	pub threads: u16,
}

impl Default for EncoderSettings {
	fn default() -> Self {
		Self {
			codec: Codec::default(),
			bitrate: 2_000_000,
			max_fps: 30.0,
			keyframe_interval: Duration::from_secs(2),
			threads: 0,
		}
	}
}

impl EncoderSettings {
	pub(crate) fn threads(&self) -> u32 {
		match self.threads {
			0 => std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u32),
			n => n as u32,
		}
	}

	/// Encoder-internal GOP length, kept longer than our time-based interval so the
	/// forced keyframe is what normally starts a group.
	pub(crate) fn gop_frames(&self) -> u32 {
		(self.max_fps * self.keyframe_interval.as_secs_f32() * 2.0)
			.ceil()
			.max(1.0) as u32
	}
}

/// One encoded frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
	/// VP8 frame, or an Annex-B H.264 access unit.
	pub data: Bytes,
	pub keyframe: bool,
	pub width: u32,
	pub height: u32,
	pub timestamp: Duration,
	/// Catalog codec description (for H.264, parsed from the keyframe's SPS).
	pub codec: VideoCodec,
}

/// A codec implementation behind [`Encoder`].
pub(crate) trait EncoderBackend: Send {
	/// Encode one frame at `pts` microseconds. Returns `(data, keyframe)`, or `None` if dropped.
	fn encode(&mut self, frame: &I420, pts: u64, force_keyframe: bool) -> Result<Option<(Bytes, bool)>>;

	/// Catalog codec description for an encoded keyframe.
	fn catalog_codec(&self, keyframe: &[u8]) -> VideoCodec;
}

/// A codec implementation behind [`Decoder`].
pub(crate) trait DecoderBackend: Send {
	/// Decode one frame into packed `rgb8` `(width, height, pixels)`.
	fn decode(&mut self, data: &[u8]) -> Result<Option<(u32, u32, Vec<u8>)>>;
}

/// Stateful encoder that accepts [`Image`]s of any supported pixel format.
///
/// The codec is (re)created lazily whenever the input resolution changes.
pub struct Encoder {
	settings: EncoderSettings,
	inner: Option<(Box<dyn EncoderBackend>, (usize, usize))>,
	/// Stream time (pts) of the last keyframe.
	last_keyframe: Option<u64>,
	/// Catalog codec of the last keyframe, reused for the delta frames that follow it.
	codec: Option<VideoCodec>,
	clock: StreamClock,
}

impl Encoder {
	/// Fails if `settings.codec` isn't compiled into this build.
	pub fn new(settings: EncoderSettings) -> Result<Self> {
		if !settings.codec.is_available() {
			return Err(unavailable(settings.codec));
		}
		Ok(Self {
			settings,
			inner: None,
			last_keyframe: None,
			codec: None,
			clock: StreamClock::default(),
		})
	}

	pub fn settings(&self) -> &EncoderSettings {
		&self.settings
	}

	/// Request that the next frame is encoded as a keyframe.
	pub fn force_keyframe(&mut self) {
		self.last_keyframe = None;
	}

	/// Encode one image. Returns `None` when the encoder dropped the frame.
	pub fn encode(&mut self, image: &Image) -> Result<Option<EncodedFrame>> {
		let yuv = I420::from_image(image)?;
		let dims = (yuv.width(), yuv.height());

		if self.inner.as_ref().map(|(_, d)| *d) != Some(dims) {
			tracing::debug!(codec = %self.settings.codec, width = dims.0, height = dims.1, "creating encoder");
			self.inner = Some((create_encoder(&self.settings, dims)?, dims));
			self.last_keyframe = None;
			self.codec = None;
		}
		let (backend, _) = self.inner.as_mut().expect("encoder initialized above");

		// Codecs want strictly increasing presentation times. Keyframes are scheduled on
		// the same timeline, so capture clock jumps can't stall or flood them.
		let pts = self.clock.micros(image.timestamp);
		let interval = self.settings.keyframe_interval.as_micros() as u64;
		let force = self
			.last_keyframe
			.is_none_or(|last| pts.saturating_sub(last) >= interval);

		let Some((data, keyframe)) = backend.encode(&yuv, pts, force)? else {
			return Ok(None);
		};
		if keyframe {
			self.last_keyframe = Some(pts);
		}
		let codec = match &self.codec {
			Some(codec) if !keyframe => codec.clone(),
			_ => self.codec.insert(backend.catalog_codec(&data)).clone(),
		};

		Ok(Some(EncodedFrame {
			data,
			keyframe,
			width: dims.0 as u32,
			height: dims.1 as u32,
			timestamp: image.timestamp,
			codec,
		}))
	}
}

/// Decoder producing `rgb8` [`Image`]s.
pub struct Decoder {
	codec: Codec,
	inner: Box<dyn DecoderBackend>,
}

impl Decoder {
	pub fn new(codec: Codec) -> Result<Self> {
		let inner: Box<dyn DecoderBackend> = match codec {
			#[cfg(feature = "vp8")]
			Codec::Vp8 => Box::new(vp8::Vp8Decoder::new()?),
			#[cfg(feature = "h264")]
			Codec::H264 => Box::new(h264::H264Decoder::new()?),
			#[allow(unreachable_patterns)]
			other => return Err(unavailable(other)),
		};
		Ok(Self { codec, inner })
	}

	pub fn codec(&self) -> Codec {
		self.codec
	}

	/// Decode one frame. Returns `None` if no picture is ready yet (e.g. while waiting for a keyframe).
	pub fn decode(&mut self, data: &[u8], timestamp: Duration) -> Result<Option<Image>> {
		let Some((width, height, rgb)) = self.inner.decode(data)? else {
			return Ok(None);
		};
		Ok(Some(Image::new(width, height, PixelFormat::Rgb8, rgb, timestamp)?))
	}
}

fn create_encoder(settings: &EncoderSettings, (width, height): (usize, usize)) -> Result<Box<dyn EncoderBackend>> {
	Ok(match settings.codec {
		#[cfg(feature = "vp8")]
		Codec::Vp8 => Box::new(vp8::Vp8Encoder::new(settings, width as u32, height as u32)?),
		#[cfg(feature = "h264")]
		Codec::H264 => Box::new(h264::H264Encoder::new(settings)?),
		#[allow(unreachable_patterns)]
		other => {
			let _ = (width, height);
			return Err(unavailable(other));
		}
	})
}

fn unavailable(codec: Codec) -> Error {
	Error::CodecUnavailable(format!(
		"{codec} support is not compiled in (enable the `{codec}` feature)"
	))
}

/// Iterate the NAL units (without start codes) of an Annex-B byte stream.
pub(crate) fn annexb_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
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
pub(crate) mod tests {
	use super::*;

	pub(crate) fn test_pattern(w: u32, h: u32, t: u32) -> Image {
		let rgb: Vec<u8> = (0..h)
			.flat_map(|y| (0..w).flat_map(move |x| [((x + t * 4) % 256) as u8, (y % 256) as u8, 100]))
			.collect();
		Image::new(w, h, PixelFormat::Rgb8, rgb, Duration::from_millis(t as u64 * 33)).unwrap()
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
				let err = out
					.data
					.iter()
					.zip(image.data.iter())
					.map(|(a, b)| a.abs_diff(*b) as u64)
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
			let (width, height) = (33, 25);
			let data = vec![128u8; width * height * format.bytes_per_pixel()];
			let timestamp = Duration::from_millis(t as u64 * 33);
			let image = Image::new(width as u32, height as u32, format, data, timestamp).unwrap();
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

	#[test]
	fn nal_splitter_handles_3_and_4_byte_start_codes() {
		let data = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
		let nals: Vec<&[u8]> = annexb_nals(&data).collect();
		assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
	}

	#[test]
	fn codec_names() {
		assert_eq!("vp8".parse::<Codec>().unwrap(), Codec::Vp8);
		assert_eq!("H264".parse::<Codec>().unwrap(), Codec::H264);
		assert!("av1".parse::<Codec>().is_err());
		assert!(Codec::Vp8.is_keyframe(&[0x10, 0x02, 0x00]));
		assert!(!Codec::Vp8.is_keyframe(&[0x11, 0x02, 0x00]));
	}

	#[test]
	fn nal_splitter_ignores_leading_bytes_and_streams_without_start_codes() {
		assert_eq!(annexb_nals(&[]).count(), 0);
		assert_eq!(annexb_nals(&[1, 2, 3, 0, 0]).count(), 0);
		let nals: Vec<&[u8]> = annexb_nals(&[9, 9, 0, 0, 1, 0x41, 5]).collect();
		assert_eq!(nals, vec![&[0x41, 5][..]]);
	}

	#[test]
	fn codec_names_round_trip() {
		for codec in [Codec::Vp8, Codec::H264] {
			assert_eq!(codec.to_string().parse::<Codec>().unwrap(), codec);
		}
		assert_eq!("AVC".parse::<Codec>().unwrap(), Codec::H264);
		assert!(matches!("".parse::<Codec>(), Err(Error::CodecUnavailable(_))));
	}

	#[test]
	fn codecs_from_catalog() {
		assert_eq!(Codec::from_catalog(&VideoCodec::VP8), Some(Codec::Vp8));
		let avc: VideoCodec = "avc3.42001f".parse().unwrap();
		assert_eq!(Codec::from_catalog(&avc), Some(Codec::H264));
		assert_eq!(Codec::from_catalog(&VideoCodec::Unknown("av01.0.04M.08".into())), None);
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
		for codec in [Codec::Vp8, Codec::H264] {
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
}
