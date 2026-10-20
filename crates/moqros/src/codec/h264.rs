//! H.264 via Cisco's prebuilt OpenH264 shared library.
//!
//! moqros never compiles OpenH264 from source: Cisco pays the MPEG LA royalties only
//! for binaries downloaded from Cisco, so the library is loaded at runtime from a path
//! given by [`set_openh264_library`] or the `OPENH264_LIBRARY` environment variable.
//! `scripts/fetch_openh264.sh` downloads the right binary for the current platform.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use openh264::OpenH264API;
use openh264::decoder::{Decoder, DecoderConfig};
use openh264::encoder::{
	BitRate, Complexity, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile, RateControlMode,
	UsageType,
};
use openh264::formats::YUVSource;

use super::{DecoderBackend, EncodedChunk, EncoderBackend, EncoderSettings, rgb8_picture};
use crate::convert::{I420, Planes};
use crate::{Error, Result};

/// Environment variable naming Cisco's OpenH264 shared library.
pub const OPENH264_LIBRARY_ENV: &str = "OPENH264_LIBRARY";

static LIBRARY: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Use Cisco's OpenH264 binary at `path` for H.264. Overrides `OPENH264_LIBRARY`.
///
/// The file's SHA-256 must match a known Cisco release.
pub fn set_openh264_library(path: impl AsRef<Path>) {
	*LIBRARY.lock().unwrap() = Some(path.as_ref().to_path_buf());
}

fn load_api() -> Result<OpenH264API> {
	let path = LIBRARY
		.lock()
		.unwrap()
		.clone()
		.or_else(|| std::env::var_os(OPENH264_LIBRARY_ENV).map(PathBuf::from))
		.ok_or_else(|| {
			Error::CodecUnavailable(format!(
				"H.264 needs Cisco's OpenH264 binary: run scripts/fetch_openh264.sh and set {OPENH264_LIBRARY_ENV}"
			))
		})?;
	OpenH264API::from_blob_path(&path)
		.map_err(|err| Error::CodecUnavailable(format!("failed to load {}: {err}", path.display())))
}

fn codec_error(err: openh264::Error) -> Error {
	Error::Codec(format!("openh264: {err}"))
}

struct Source<'a>(&'a I420);

impl YUVSource for Source<'_> {
	fn dimensions(&self) -> (usize, usize) {
		(self.0.width(), self.0.height())
	}

	fn strides(&self) -> (usize, usize, usize) {
		let w = self.0.width();
		(w, w / 2, w / 2)
	}

	fn y(&self) -> &[u8] {
		self.0.y()
	}

	fn u(&self) -> &[u8] {
		self.0.u()
	}

	fn v(&self) -> &[u8] {
		self.0.v()
	}
}

pub(crate) struct H264Encoder {
	inner: Encoder,
}

impl H264Encoder {
	pub fn new(settings: &EncoderSettings) -> Result<Self> {
		let config = EncoderConfig::new()
			.usage_type(UsageType::CameraVideoRealTime)
			.profile(Profile::Baseline)
			.complexity(Complexity::Low)
			.rate_control_mode(RateControlMode::Bitrate)
			.bitrate(BitRate::from_bps(settings.bitrate))
			.max_frame_rate(FrameRate::from_hz(settings.max_fps))
			.intra_frame_period(IntraFramePeriod::from_num_frames(settings.gop_frames()))
			.skip_frames(true)
			.num_threads(u16::try_from(settings.threads()).unwrap_or(u16::MAX));
		let inner = Encoder::with_api_config(load_api()?, config).map_err(codec_error)?;
		Ok(Self { inner })
	}
}

impl EncoderBackend for H264Encoder {
	fn encode(&mut self, frame: &I420, _pts: u64, force_keyframe: bool) -> Result<Option<EncodedChunk>> {
		if force_keyframe {
			self.inner.force_intra_frame();
		}
		let bitstream = self.inner.encode(&Source(frame)).map_err(codec_error)?;
		// Only an IDR resets decoder state, so only it may start a MoQ group; a plain
		// I frame can still be referenced across by later frames.
		let keyframe = match bitstream.frame_type() {
			FrameType::IDR => true,
			FrameType::I | FrameType::P | FrameType::IPMixed => false,
			FrameType::Skip | FrameType::Invalid => return Ok(None),
		};
		let data = bitstream.to_vec();
		Ok((!data.is_empty()).then(|| EncodedChunk {
			data: data.into(),
			keyframe,
		}))
	}
}

pub(crate) struct H264Decoder {
	inner: Decoder,
}

impl H264Decoder {
	pub fn new() -> Result<Self> {
		let inner = Decoder::with_api_config(load_api()?, DecoderConfig::new()).map_err(codec_error)?;
		Ok(Self { inner })
	}
}

impl DecoderBackend for H264Decoder {
	fn decode(&mut self, data: &[u8], max_pixels: u64) -> Result<Option<(u32, u32, Vec<u8>)>> {
		// OpenH264 has allocated the picture by now, but the RGB copies aren't made.
		let Some(yuv) = self.inner.decode(data).map_err(codec_error)? else {
			return Ok(None);
		};
		let (width, height) = yuv.dimensions();
		let (ys, us, vs) = yuv.strides();
		let planes = Planes {
			width,
			height,
			y: (yuv.y(), ys),
			u: (yuv.u(), us),
			v: (yuv.v(), vs),
		};
		Ok(Some(rgb8_picture(&planes, max_pixels)?))
	}
}

/// These tests need Cisco's binary: `OPENH264_LIBRARY=... cargo test --features h264`.
#[cfg(test)]
mod tests {
	use super::super::{Codec, tests};

	fn available() -> bool {
		let ok = std::env::var_os(super::OPENH264_LIBRARY_ENV).is_some();
		if !ok {
			eprintln!("OPENH264_LIBRARY not set; skipping H.264 test");
		}
		ok
	}

	#[test]
	fn round_trip() {
		if available() {
			tests::round_trip(Codec::H264);
		}
	}

	#[test]
	fn keyframes() {
		if available() {
			tests::keyframes(Codec::H264);
		}
	}

	#[test]
	fn catalog_codec_from_sps() {
		if available() {
			let mut encoder = super::super::Encoder::new(super::super::EncoderSettings {
				codec: Codec::H264,
				..Default::default()
			})
			.unwrap();
			let frame = encoder.encode(&tests::test_pattern(64, 48, 0)).unwrap().unwrap();
			assert!(frame.codec.to_string().starts_with("avc3.42"), "{}", frame.codec);
		}
	}

	#[test]
	fn pixel_formats() {
		if available() {
			tests::pixel_formats(Codec::H264);
		}
	}

	#[test]
	fn irregular_timestamps() {
		if available() {
			tests::irregular_timestamps(Codec::H264);
		}
	}

	#[test]
	fn forced_keyframe() {
		if available() {
			tests::forced_keyframe(Codec::H264);
		}
	}

	#[test]
	fn rejects_invalid_images() {
		if available() {
			tests::rejects_invalid_images(Codec::H264);
		}
	}

	#[test]
	fn rejects_oversized_pictures() {
		if available() {
			tests::rejects_oversized_pictures(Codec::H264);
		}
	}

	#[test]
	fn keyframe_after_clock_jump() {
		if available() {
			tests::keyframe_after_clock_jump(Codec::H264);
		}
	}
}
