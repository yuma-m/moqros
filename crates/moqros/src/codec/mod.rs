//! Video encoding/decoding.
//!
//! - **VP8** (feature `vp8`, default): royalty-free, via the system libvpx.
//! - **AV1** (feature `av1`): royalty-free, via the system libaom.
//! - **H.264** (feature `h264`): via Cisco's prebuilt OpenH264 binary, loaded at
//!   runtime so Cisco's patent license applies. See [`set_openh264_library`].
//!
//! With the `hw` feature, encoders run on the GPU through GStreamer when one is available
//! (see [`Hardware`]); decoding always runs in software.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use bytes::Bytes;
use hang::catalog::VideoCodec;

use crate::clock::StreamClock;
use crate::convert::{I420, Planes, yuv_to_rgb8};
use crate::{Error, Image, PixelFormat, Result};

mod annexb;
#[cfg(feature = "av1")]
mod av1;
#[cfg(feature = "hw")]
mod gst;
#[cfg(feature = "h264")]
mod h264;
mod obu;
#[cfg(feature = "vp8")]
mod vp8;
mod vp8_header;

#[cfg(feature = "h264")]
pub use h264::set_openh264_library;

/// Video codecs moqros can produce and consume.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Codec {
	Vp8,
	H264,
	Av1,
}

impl Codec {
	/// Every codec moqros knows, compiled in or not.
	#[cfg(test)]
	pub(crate) const ALL: &[Codec] = &[Codec::Vp8, Codec::H264, Codec::Av1];

	/// Codecs compiled into this build, preferred first.
	pub const fn available() -> &'static [Codec] {
		&[
			#[cfg(feature = "vp8")]
			Codec::Vp8,
			#[cfg(feature = "av1")]
			Codec::Av1,
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
			VideoCodec::AV1(_) => Some(Self::Av1),
			_ => None,
		}
	}

	/// Whether `data` (one encoded frame) is a keyframe.
	pub fn is_keyframe(self, data: &[u8]) -> bool {
		match self {
			Self::Vp8 => vp8_header::is_keyframe(data),
			Self::H264 => annexb::is_keyframe(data),
			Self::Av1 => obu::is_keyframe(data),
		}
	}

	/// Catalog codec description for an encoded keyframe (for H.264 read from its SPS, for
	/// AV1 from its sequence header).
	pub(crate) fn catalog_codec(self, keyframe: &[u8]) -> VideoCodec {
		match self {
			Self::Vp8 => VideoCodec::VP8,
			Self::H264 => annexb::catalog_codec(keyframe),
			Self::Av1 => obu::catalog_codec(keyframe),
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
			Self::Av1 => "av1",
		})
	}
}

impl FromStr for Codec {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self> {
		match s.to_ascii_lowercase().as_str() {
			"vp8" => Ok(Self::Vp8),
			"h264" | "avc" => Ok(Self::H264),
			"av1" | "av01" => Ok(Self::Av1),
			other => Err(Error::CodecUnavailable(format!("unknown codec {other:?}"))),
		}
	}
}

/// Whether [`Encoder`] encodes on a hardware encoder (feature `hw`).
///
/// Hardware encoders are reached through GStreamer: VA-API (`va` and `vaapi` plugins) on
/// Intel/AMD GPUs and NVENC (`nvcodec` plugin) on NVIDIA GPUs. Software encoding stays the
/// fallback, so the codec's own feature is still needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Hardware {
	/// Always encode in software.
	Off,
	/// Use a hardware encoder when one works, otherwise encode in software.
	#[default]
	Auto,
	/// Fail unless a hardware encoder works.
	Require,
}

impl fmt::Display for Hardware {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(match self {
			Self::Off => "off",
			Self::Auto => "auto",
			Self::Require => "require",
		})
	}
}

impl FromStr for Hardware {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self> {
		match s.to_ascii_lowercase().as_str() {
			"off" => Ok(Self::Off),
			"auto" => Ok(Self::Auto),
			"require" => Ok(Self::Require),
			other => Err(Error::InvalidSettings(format!(
				"unknown hardware mode {other:?} (expected off, auto, or require)"
			))),
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
	/// so this bounds how long a late joiner waits for the first picture. A group
	/// nearing moq-net's limits (8192 frames or 32 MB) gets an earlier keyframe.
	pub keyframe_interval: Duration,
	/// Encoder worker threads; 0 picks a sensible default.
	pub threads: u16,
	/// Whether to encode on a hardware encoder.
	pub hardware: Hardware,
}

impl Default for EncoderSettings {
	fn default() -> Self {
		Self {
			codec: Codec::default(),
			bitrate: 2_000_000,
			max_fps: 30.0,
			keyframe_interval: Duration::from_secs(2),
			threads: 0,
			hardware: Hardware::default(),
		}
	}
}

impl EncoderSettings {
	pub(crate) fn threads(&self) -> u32 {
		match self.threads {
			0 => default_threads(),
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

/// Codec worker threads when none are configured: one per core, at most 4.
pub(crate) fn default_threads() -> u32 {
	std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u32)
}

/// One encoded frame.
#[derive(Debug, Clone)]
pub struct EncodedFrame {
	/// VP8 frame, Annex-B H.264 access unit, or AV1 temporal unit (low-overhead OBUs).
	pub data: Bytes,
	pub keyframe: bool,
	pub width: u32,
	pub height: u32,
	pub timestamp: Duration,
	/// Catalog codec description (for H.264 parsed from the keyframe's SPS, for AV1 from its
	/// sequence header).
	pub codec: VideoCodec,
}

/// What an [`EncoderBackend`] produced for one input frame.
pub(crate) struct EncodedChunk {
	pub data: Bytes,
	pub keyframe: bool,
}

/// A codec implementation behind [`Encoder`].
pub(crate) trait EncoderBackend: Send {
	/// Encode one frame at `pts` microseconds. Returns `None` if the encoder dropped it.
	fn encode(&mut self, frame: &I420, pts: u64, force_keyframe: bool) -> Result<Option<EncodedChunk>>;
}

/// A codec implementation behind [`Decoder`].
pub(crate) trait DecoderBackend: Send {
	/// Decode one frame into packed `rgb8` `(width, height, pixels)` (see [`rgb8_picture`]),
	/// failing with [`Error::TooLarge`] for pictures above `max_pixels`.
	fn decode(&mut self, data: &[u8], max_pixels: u64) -> Result<Option<(u32, u32, Vec<u8>)>>;
}

/// A decoder's output picture as packed `rgb8` `(width, height, pixels)`, failing with
/// [`Error::TooLarge`] before converting a picture above `max_pixels`.
pub(crate) fn rgb8_picture(planes: &Planes<'_>, max_pixels: u64) -> Result<(u32, u32, Vec<u8>)> {
	let (width, height) = (planes.width as u32, planes.height as u32);
	check_size(width, height, max_pixels)?;
	Ok((width, height, yuv_to_rgb8(planes)))
}

/// Default for [`Decoder::set_max_pixels`]: 8K UHD (7680x4320).
pub const DEFAULT_MAX_PIXELS: u64 = 7680 * 4320;

/// Fail with [`Error::TooLarge`] if a `width`x`height` picture is above `max` pixels.
pub(crate) fn check_size(width: u32, height: u32, max: u64) -> Result<()> {
	if width as u64 * height as u64 > max {
		return Err(Error::TooLarge { width, height, max });
	}
	Ok(())
}

/// Stateful encoder that accepts [`Image`]s of any supported pixel format.
///
/// The codec is (re)created lazily whenever the input resolution changes.
pub struct Encoder {
	settings: EncoderSettings,
	active: Option<ActiveBackend>,
	/// Hardware encoder in use, if any. Outlives a backend that's recreated, so the same
	/// element is tried first for the new one.
	hardware: Option<&'static str>,
	/// Stream time (pts) of the last keyframe.
	last_keyframe: Option<u64>,
	clock: StreamClock,
}

/// The backend an [`Encoder`] currently encodes with, and what it has produced so far.
struct ActiveBackend {
	backend: Box<dyn EncoderBackend>,
	dims: (usize, usize),
	/// Catalog codec of the last keyframe, reused for the delta frames that follow it.
	codec: Option<VideoCodec>,
}

impl ActiveBackend {
	fn new(backend: Box<dyn EncoderBackend>, dims: (usize, usize)) -> Self {
		Self {
			backend,
			dims,
			codec: None,
		}
	}
}

impl Encoder {
	/// Fails if `settings.codec` isn't compiled into this build, or if `settings.hardware` is
	/// [`Hardware::Require`] without the `hw` feature.
	pub fn new(settings: EncoderSettings) -> Result<Self> {
		if !settings.codec.is_available() {
			return Err(unavailable(settings.codec));
		}
		if settings.hardware == Hardware::Require && !cfg!(feature = "hw") {
			return Err(Error::CodecUnavailable(
				"hardware encoding is not compiled in (enable the `hw` feature)".into(),
			));
		}
		Ok(Self {
			settings,
			active: None,
			hardware: None,
			last_keyframe: None,
			clock: StreamClock::default(),
		})
	}

	pub fn settings(&self) -> &EncoderSettings {
		&self.settings
	}

	/// Name of the GStreamer element encoding on hardware, once the first frame picked one.
	/// `None` while encoding in software.
	pub fn hardware_encoder(&self) -> Option<&'static str> {
		self.hardware
	}

	/// Request that the next frame is encoded as a keyframe.
	pub fn force_keyframe(&mut self) {
		self.last_keyframe = None;
	}

	/// Encode one image. Returns `None` when the encoder dropped the frame.
	pub fn encode(&mut self, image: &Image) -> Result<Option<EncodedFrame>> {
		let yuv = I420::from_image(image)?;
		let dims = (yuv.width(), yuv.height());

		if self.active.as_ref().is_none_or(|active| active.dims != dims) {
			tracing::debug!(codec = %self.settings.codec, width = dims.0, height = dims.1, "creating encoder");
			// Drop the old encoder first: a hardware encoder may only allow one session.
			self.active = None;
			self.active = Some(ActiveBackend::new(self.create_backend(dims)?, dims));
			self.last_keyframe = None;
		}

		// Codecs want strictly increasing presentation times. Keyframes are scheduled on
		// the same timeline, so capture clock jumps can't stall or flood them.
		let pts = self.clock.micros(image.timestamp);
		let interval = self.settings.keyframe_interval.as_micros() as u64;
		let force = self
			.last_keyframe
			.is_none_or(|last| pts.saturating_sub(last) >= interval);

		let Some(EncodedChunk { data, keyframe }) = self.encode_with_fallback(&yuv, pts, force)? else {
			return Ok(None);
		};
		if keyframe {
			self.last_keyframe = Some(pts);
		}
		let active = self.active.as_mut().expect("backend created above");
		let codec = match &active.codec {
			Some(codec) if !keyframe => codec.clone(),
			_ => active.codec.insert(self.settings.codec.catalog_codec(&data)).clone(),
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

	/// Encode on the active backend.
	///
	/// A hardware encoder that fails mid-stream (e.g. a GPU reset) is replaced by a software
	/// one in auto mode, and the frame is encoded again as a keyframe.
	fn encode_with_fallback(&mut self, yuv: &I420, pts: u64, force: bool) -> Result<Option<EncodedChunk>> {
		let active = self.active.as_mut().expect("backend created before encoding");
		let dims = active.dims;
		match active.backend.encode(yuv, pts, force) {
			Ok(chunk) => Ok(chunk),
			Err(err) if self.hardware.is_some() && self.settings.hardware == Hardware::Auto => {
				tracing::warn!(codec = %self.settings.codec, element = self.hardware, %err, "hardware encoder failed; encoding in software");
				self.hardware = None;
				self.active = None;
				let backend = create_software_encoder(&self.settings, dims)?;
				let active = self.active.insert(ActiveBackend::new(backend, dims));
				active.backend.encode(yuv, pts, true)
			}
			Err(err) => {
				// Recreate the encoder on the next frame rather than reuse a broken one.
				if self.hardware.is_some() {
					self.active = None;
				}
				Err(err)
			}
		}
	}

	/// A hardware encoder if one works (and is wanted), otherwise a software one.
	///
	/// Hardware encoders may only reject some sizes, so they're tried again for a new
	/// resolution; ones that failed at a size aren't (see [`gst::GstEncoder::new`]).
	fn create_backend(&mut self, dims: (usize, usize)) -> Result<Box<dyn EncoderBackend>> {
		#[cfg(feature = "hw")]
		if self.settings.hardware != Hardware::Off {
			let first_attempt = gst::first_attempt(self.settings.codec, dims);
			match gst::GstEncoder::new(&self.settings, dims, self.hardware) {
				Ok(encoder) => {
					if self.hardware != Some(encoder.element()) {
						tracing::info!(codec = %self.settings.codec, element = encoder.element(), "using hardware encoder");
					}
					self.hardware = Some(encoder.element());
					return Ok(Box::new(encoder));
				}
				Err(err) if self.settings.hardware == Hardware::Require => return Err(err),
				Err(err) => {
					// Warn once per codec and size, not for every topic.
					if first_attempt {
						tracing::warn!(codec = %self.settings.codec, %err, "no usable hardware encoder; encoding in software");
					} else {
						tracing::debug!(codec = %self.settings.codec, %err, "no usable hardware encoder; encoding in software");
					}
					self.hardware = None;
				}
			}
		}
		create_software_encoder(&self.settings, dims)
	}
}

/// Decoder producing `rgb8` [`Image`]s.
///
/// A bitstream can declare a huge picture in a few bytes, so pictures above
/// [`Decoder::max_pixels`] are rejected rather than allocated.
pub struct Decoder {
	codec: Codec,
	inner: Box<dyn DecoderBackend>,
	max_pixels: u64,
}

impl Decoder {
	pub fn new(codec: Codec) -> Result<Self> {
		let inner: Box<dyn DecoderBackend> = match codec {
			#[cfg(feature = "vp8")]
			Codec::Vp8 => Box::new(vp8::Vp8Decoder::new()?),
			#[cfg(feature = "h264")]
			Codec::H264 => Box::new(h264::H264Decoder::new()?),
			#[cfg(feature = "av1")]
			Codec::Av1 => Box::new(av1::Av1Decoder::new()?),
			#[allow(unreachable_patterns)]
			other => return Err(unavailable(other)),
		};
		Ok(Self {
			codec,
			inner,
			max_pixels: DEFAULT_MAX_PIXELS,
		})
	}

	pub fn codec(&self) -> Codec {
		self.codec
	}

	/// Largest picture, in pixels, [`Decoder::decode`] accepts.
	pub fn max_pixels(&self) -> u64 {
		self.max_pixels
	}

	/// Accept pictures of up to `max_pixels` (default [`DEFAULT_MAX_PIXELS`]).
	pub fn set_max_pixels(&mut self, max_pixels: u64) {
		self.max_pixels = max_pixels;
	}

	/// Decode one frame. Returns `None` if no picture is ready yet (e.g. while waiting for a keyframe).
	///
	/// Fails with [`Error::TooLarge`] for a picture above [`Decoder::max_pixels`].
	pub fn decode(&mut self, data: &[u8], timestamp: Duration) -> Result<Option<Image>> {
		let Some((width, height, rgb)) = self.inner.decode(data, self.max_pixels)? else {
			return Ok(None);
		};
		Ok(Some(Image::new(width, height, PixelFormat::Rgb8, rgb, timestamp)?))
	}
}

fn create_software_encoder(
	settings: &EncoderSettings,
	(width, height): (usize, usize),
) -> Result<Box<dyn EncoderBackend>> {
	Ok(match settings.codec {
		#[cfg(feature = "vp8")]
		Codec::Vp8 => Box::new(vp8::Vp8Encoder::new(settings, width as u32, height as u32)?),
		#[cfg(feature = "h264")]
		Codec::H264 => Box::new(h264::H264Encoder::new(settings)?),
		#[cfg(feature = "av1")]
		Codec::Av1 => Box::new(av1::Av1Encoder::new(settings, width as u32, height as u32)?),
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
		assert!(matches!("".parse::<Codec>(), Err(Error::CodecUnavailable(_))));
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
}
