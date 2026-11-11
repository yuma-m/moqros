//! Video encoding/decoding.
//!
//! - **VP8** (feature `vp8`, default): royalty-free, via the system libvpx.
//! - **AV1** (feature `av1`): royalty-free, via the system libaom.
//! - **H.264** (feature `h264`): via Cisco's prebuilt OpenH264 binary, loaded at
//!   runtime so Cisco's patent license applies. See [`set_openh264_library`].
//!
//! With the `hw` feature, encoders run on the GPU through GStreamer when one is available
//! (see [`Hardware`]); decoding always runs in software.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::VideoCodec;

use crate::clock::StreamClock;
use crate::convert::{I420, Planes, yuv_to_packed};
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
	/// VP8 (feature `vp8`, on by default).
	Vp8,
	/// H.264 (feature `h264`).
	H264,
	/// AV1 (feature `av1`).
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
	pub(crate) fn from_catalog(codec: &VideoCodec) -> Option<Self> {
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

crate::names::named_enum!(Codec, "codec", {
	Vp8 => "vp8",
	H264 => "h264" | "avc",
	Av1 => "av1" | "av01",
});

impl Default for Codec {
	fn default() -> Self {
		Self::available().first().copied().unwrap_or(Self::Vp8)
	}
}

/// Whether [`Encoder`] encodes on a hardware encoder (feature `hw`).
///
/// Hardware encoders are reached through GStreamer: VA-API (`va` and `vaapi` plugins) on
/// Intel/AMD GPUs and NVENC (`nvcodec` plugin) on NVIDIA GPUs. Software encoding stays the
/// fallback, so the codec's own feature is still needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum Hardware {
	/// Always encode in software.
	Off,
	/// Use a hardware encoder when one works, otherwise encode in software.
	#[default]
	Auto,
	/// Fail unless a hardware encoder works.
	Require,
}

crate::names::named_enum!(Hardware, "hardware mode", {
	Off => "off",
	Auto => "auto",
	Require => "require",
});

/// Tuning knobs for [`Encoder`].
#[derive(Debug, Clone)]
pub struct EncoderSettings {
	/// Codec to encode with.
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
			n => u32::from(n),
		}
	}

	/// Encoder-internal GOP length, kept longer than our time-based interval so the
	/// forced keyframe is what normally starts a group.
	#[expect(
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss,
		reason = "float casts saturate"
	)]
	pub(crate) fn gop_frames(&self) -> u32 {
		(self.max_fps * self.keyframe_interval.as_secs_f32() * 2.0)
			.ceil()
			.max(1.0) as u32
	}

	/// Duration of one frame at `max_fps`, in microseconds.
	#[expect(
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss,
		reason = "float casts saturate"
	)]
	pub(crate) fn frame_duration_micros(&self) -> u64 {
		(1_000_000.0 / self.max_fps.max(1.0)) as u64
	}
}

/// Codec worker threads when none are configured: one per core, at most 4.
pub(crate) fn default_threads() -> u32 {
	std::thread::available_parallelism().map_or(1, |n| u32::try_from(n.get()).unwrap_or(4).min(4))
}

/// One encoded frame.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct EncodedFrame {
	/// VP8 frame, Annex-B H.264 access unit, or AV1 temporal unit (low-overhead OBUs).
	pub data: Bytes,
	/// Whether the frame decodes on its own.
	pub keyframe: bool,
	/// Picture width in pixels.
	pub width: u32,
	/// Picture height in pixels.
	pub height: u32,
	/// Timestamp of the encoded [`Image`].
	pub timestamp: Duration,
	/// Codec of `data`.
	pub codec: Codec,
}

impl EncodedFrame {
	/// A frame encoded elsewhere, for [`ImagePublisher::publish_encoded`](crate::ImagePublisher::publish_encoded):
	/// `data` is one `width`x`height` picture in `codec`, captured at `timestamp`.
	pub fn new(
		codec: Codec,
		data: impl Into<Bytes>,
		keyframe: bool,
		width: u32,
		height: u32,
		timestamp: Duration,
	) -> Self {
		Self {
			data: data.into(),
			keyframe,
			width,
			height,
			timestamp,
			codec,
		}
	}

	/// The WebCodecs codec string a catalog announces for this frame, e.g. `vp8`, `avc3.42c01f`
	/// or `av01.0.05M.08` (for H.264 read from the SPS, for AV1 from the sequence header).
	/// `None` for a delta frame, which doesn't carry those.
	pub fn codec_string(&self) -> Option<String> {
		self.keyframe.then(|| self.codec.catalog_codec(&self.data).to_string())
	}
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

	/// Aim for `bitrate` bit/s from the next frame on. Returns `false` if the backend can't change
	/// it while running, so it has to be recreated.
	fn set_bitrate(&mut self, bitrate: u32) -> Result<bool> {
		let _ = bitrate;
		Ok(false)
	}
}

/// A codec implementation behind [`Decoder`].
pub(crate) trait DecoderBackend: Send {
	/// Decode one frame into packed `format` `(width, height, pixels)` (see [`packed_picture`]),
	/// failing with [`Error::TooLarge`] for pictures above `max_pixels`.
	fn decode(&mut self, data: &[u8], max_pixels: u64, format: PixelFormat) -> Result<Option<(u32, u32, Vec<u8>)>>;
}

/// A decoder's output picture as packed `format` `(width, height, pixels)`, failing with
/// [`Error::TooLarge`] before converting a picture above `max_pixels`.
pub(crate) fn packed_picture(planes: &Planes<'_>, max_pixels: u64, format: PixelFormat) -> Result<(u32, u32, Vec<u8>)> {
	// A picture too large for `u32` saturates, and then fails the size check.
	let dim = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
	let (width, height) = (dim(planes.width), dim(planes.height));
	check_size(width, height, max_pixels)?;
	Ok((width, height, yuv_to_packed(planes, format)?))
}

/// Default for [`Decoder::set_max_pixels`]: 8K UHD (7680x4320).
pub const DEFAULT_MAX_PIXELS: u64 = 7680 * 4320;

/// Fail with [`Error::TooLarge`] if a `width`x`height` picture is above `max` pixels.
pub(crate) fn check_size(width: u32, height: u32, max: u64) -> Result<()> {
	if u64::from(width) * u64::from(height) > max {
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
	/// The converted input, reused from frame to frame.
	yuv: I420,
}

/// The backend an [`Encoder`] currently encodes with, and what it has produced so far.
struct ActiveBackend {
	backend: Box<dyn EncoderBackend>,
	dims: (usize, usize),
}

impl ActiveBackend {
	fn new(backend: Box<dyn EncoderBackend>, dims: (usize, usize)) -> Self {
		Self { backend, dims }
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
			yuv: I420::default(),
		})
	}

	/// The settings this encoder was created with.
	pub fn settings(&self) -> &EncoderSettings {
		&self.settings
	}

	/// Name of the GStreamer element encoding on hardware, once the first frame picked one.
	/// `None` while encoding in software.
	pub fn hardware_encoder(&self) -> Option<&'static str> {
		self.hardware
	}

	/// Aim for `bitrate` bit/s from the next frame on.
	///
	/// VP8 and AV1 software encoders adjust in place. Other encoders (H.264, hardware) are
	/// recreated, so the next frame is a keyframe.
	pub fn set_bitrate(&mut self, bitrate: u32) {
		if bitrate == self.settings.bitrate {
			return;
		}
		self.settings.bitrate = bitrate;
		let Some(active) = self.active.as_mut() else {
			return;
		};
		match active.backend.set_bitrate(bitrate) {
			Ok(true) => tracing::debug!(codec = %self.settings.codec, bitrate, "changed the bitrate"),
			Ok(false) => {
				tracing::debug!(codec = %self.settings.codec, bitrate, "recreating the encoder for the new bitrate");
				self.active = None;
			}
			Err(err) => {
				tracing::warn!(codec = %self.settings.codec, bitrate, %err, "changing the bitrate failed; recreating the encoder");
				self.active = None;
			}
		}
	}

	/// Request that the next frame is encoded as a keyframe.
	pub fn force_keyframe(&mut self) {
		self.last_keyframe = None;
	}

	/// Encode one image. Returns `None` when the encoder dropped the frame.
	pub fn encode(&mut self, image: &Image) -> Result<Option<EncodedFrame>> {
		self.yuv.fill_from_image(image)?;
		// Taken out for the call, which borrows the encoder.
		let yuv = std::mem::take(&mut self.yuv);
		let frame = self.encode_yuv(&yuv, image.timestamp);
		self.yuv = yuv;
		frame
	}

	/// Encode one converted picture captured at `timestamp`.
	#[expect(
		clippy::cast_possible_truncation,
		reason = "I420 dimensions come from `u32` image dimensions"
	)]
	pub(crate) fn encode_yuv(&mut self, yuv: &I420, timestamp: Duration) -> Result<Option<EncodedFrame>> {
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
		let pts = self.clock.micros(timestamp);
		let interval = u64::try_from(self.settings.keyframe_interval.as_micros()).unwrap_or(u64::MAX);
		let force = self
			.last_keyframe
			.is_none_or(|last| pts.saturating_sub(last) >= interval);

		let Some(EncodedChunk { data, keyframe }) = self.encode_with_fallback(yuv, pts, force)? else {
			return Ok(None);
		};
		if keyframe {
			self.last_keyframe = Some(pts);
		}

		Ok(Some(EncodedFrame {
			data,
			keyframe,
			width: dims.0 as u32,
			height: dims.1 as u32,
			timestamp,
			codec: self.settings.codec,
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
	output: PixelFormat,
}

impl Decoder {
	/// A decoder for `codec`. Fails if the codec isn't compiled in or can't be loaded.
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
			output: PixelFormat::Rgb8,
		})
	}

	/// The codec this decoder reads.
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

	/// The pixel format of decoded images.
	pub fn output_format(&self) -> PixelFormat {
		self.output
	}

	/// Decode into `format` images (`rgb8` by default): `rgb8`, `bgr8`, `rgba8`, `bgra8` or
	/// `mono8` (the luma only). Fails with [`Error::InvalidSettings`] for the 4:2:2 formats.
	pub fn set_output_format(&mut self, format: PixelFormat) -> Result<()> {
		if matches!(format, PixelFormat::Uyvy | PixelFormat::Yuyv) {
			return Err(Error::InvalidSettings(format!("can't decode into {format}")));
		}
		self.output = format;
		Ok(())
	}

	/// Decode one frame. Returns `None` if no picture is ready yet (e.g. while waiting for a keyframe).
	///
	/// Fails with [`Error::TooLarge`] for a picture above [`Decoder::max_pixels`].
	pub fn decode(&mut self, data: &[u8], timestamp: Duration) -> Result<Option<Image>> {
		let Some((width, height, pixels)) = self.inner.decode(data, self.max_pixels, self.output)? else {
			return Ok(None);
		};
		Ok(Some(Image::new(width, height, self.output, pixels, timestamp)?))
	}
}

#[expect(
	clippy::cast_possible_truncation,
	reason = "I420 dimensions come from `u32` image dimensions"
)]
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
pub(crate) mod tests;
