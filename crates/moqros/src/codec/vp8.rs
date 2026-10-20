//! VP8 via libvpx, tuned for real-time streaming.

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint};
use std::ptr;

use bytes::BytesMut;
use vpx_sys as vpx;

use super::vp8_header::keyframe_size;
use super::{DecoderBackend, EncodedChunk, EncoderBackend, EncoderSettings, check_size, default_threads, rgb8_picture};
use crate::convert::{I420, Planes};
use crate::{Error, Result};

/// Speed/quality trade-off; negative values select real-time mode in libvpx.
/// -8 is in the range WebRTC uses for desktop-class CPUs.
const CPU_USED: c_int = -8;

/// An initialised libvpx codec context, destroyed when dropped.
struct Context(vpx::vpx_codec_ctx_t);

// SAFETY: a libvpx context may be used from any thread as long as it isn't shared;
// `Context` owns it exclusively and all access goes through `&mut self`.
unsafe impl Send for Context {}

impl Context {
	/// Initialise a context with `init`, e.g. `vpx_codec_enc_init_ver`.
	///
	/// # Safety
	///
	/// `init` must leave the context it is given initialised whenever it returns
	/// `VPX_CODEC_OK`.
	unsafe fn new(init: impl FnOnce(*mut vpx::vpx_codec_ctx_t) -> vpx::vpx_codec_err_t) -> Result<Self> {
		let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::zeroed();
		check(init(ctx.as_mut_ptr()), None)?;
		// SAFETY: `init` succeeded, so the caller vouches that it initialised `ctx`.
		Ok(Self(unsafe { ctx.assume_init() }))
	}

	/// Fail with libvpx's error (and this context's detail) unless `err` is `VPX_CODEC_OK`.
	fn check(&self, err: vpx::vpx_codec_err_t) -> Result<()> {
		check(err, Some(&self.0))
	}
}

impl Drop for Context {
	fn drop(&mut self) {
		// SAFETY: the context was initialised in `new` and is not used again.
		unsafe {
			vpx::vpx_codec_destroy(&mut self.0);
		}
	}
}

pub(crate) struct Vp8Encoder {
	ctx: Context,
	width: u32,
	height: u32,
	frame_duration: u64,
}

impl Vp8Encoder {
	#[expect(clippy::cast_possible_wrap, reason = "libvpx's ABI version is a small constant")]
	pub fn new(settings: &EncoderSettings, width: u32, height: u32) -> Result<Self> {
		// SAFETY: libvpx initialises `cfg` before it is assumed initialised, and the context once
		// `vpx_codec_enc_init_ver` succeeds.
		unsafe {
			let iface = vpx::vpx_codec_vp8_cx();
			let mut cfg = MaybeUninit::<vpx::vpx_codec_enc_cfg_t>::zeroed();
			check(vpx::vpx_codec_enc_config_default(iface, cfg.as_mut_ptr(), 0), None)?;
			let mut cfg = cfg.assume_init();

			cfg.g_w = width;
			cfg.g_h = height;
			// Timestamps are passed in microseconds.
			cfg.g_timebase = vpx::vpx_rational { num: 1, den: 1_000_000 };
			cfg.g_threads = settings.threads();
			cfg.g_lag_in_frames = 0;
			cfg.g_error_resilient = vpx::VPX_ERROR_RESILIENT_DEFAULT;
			cfg.rc_end_usage = vpx::vpx_rc_mode::VPX_CBR;
			cfg.rc_target_bitrate = (settings.bitrate / 1000).max(1);
			cfg.rc_min_quantizer = 4;
			cfg.rc_max_quantizer = 56;
			cfg.rc_dropframe_thresh = 0;
			cfg.kf_mode = vpx::vpx_kf_mode::VPX_KF_AUTO;
			cfg.kf_max_dist = settings.gop_frames();

			let mut ctx = Context::new(|ctx| {
				vpx::vpx_codec_enc_init_ver(ctx, iface, &cfg, 0, vpx::VPX_ENCODER_ABI_VERSION as c_int)
			})?;
			let err = vpx::vpx_codec_control_(
				&mut ctx.0,
				vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int,
				CPU_USED,
			);
			ctx.check(err)?;

			Ok(Self {
				ctx,
				width,
				height,
				frame_duration: settings.frame_duration_micros(),
			})
		}
	}
}

impl EncoderBackend for Vp8Encoder {
	fn encode(&mut self, frame: &I420, pts: u64, force_keyframe: bool) -> Result<Option<EncodedChunk>> {
		// libvpx reads a whole picture of the configured size from the frame's buffer.
		if (frame.width(), frame.height()) != (self.width as usize, self.height as usize) {
			return Err(Error::Codec(format!(
				"{}x{} frame for a {}x{} encoder",
				frame.width(),
				frame.height(),
				self.width,
				self.height
			)));
		}
		let pts = i64::try_from(pts).map_err(|_| Error::Codec(format!("timestamp {pts} out of range")))?;

		let mut out = BytesMut::new();
		let mut keyframe = false;
		// SAFETY: `frame` has the dimensions the encoder was configured with and outlives the
		// wrapped image; packets are only read until the next call into the encoder.
		unsafe {
			let mut img = MaybeUninit::<vpx::vpx_image_t>::zeroed();
			// libvpx only reads from the wrapped buffer when encoding.
			let wrapped = vpx::vpx_img_wrap(
				img.as_mut_ptr(),
				vpx::vpx_img_fmt::VPX_IMG_FMT_I420,
				self.width,
				self.height,
				1,
				frame.data().as_ptr().cast_mut(),
			);
			if wrapped.is_null() {
				return Err(Error::Codec("vpx_img_wrap failed".into()));
			}

			let flags = if force_keyframe {
				vpx::VPX_EFLAG_FORCE_KF.into()
			} else {
				0
			};
			let err = vpx::vpx_codec_encode(
				&mut self.ctx.0,
				wrapped,
				pts,
				self.frame_duration as _,
				flags,
				vpx::VPX_DL_REALTIME.into(),
			);
			self.ctx.check(err)?;

			let mut iter: vpx::vpx_codec_iter_t = ptr::null();
			loop {
				let pkt = vpx::vpx_codec_get_cx_data(&mut self.ctx.0, &mut iter);
				if pkt.is_null() {
					break;
				}
				if (*pkt).kind != vpx::vpx_codec_cx_pkt_kind::VPX_CODEC_CX_FRAME_PKT {
					continue;
				}
				let f = (*pkt).data.frame;
				out.extend_from_slice(std::slice::from_raw_parts(f.buf as *const u8, f.sz));
				keyframe |= f.flags & vpx::VPX_FRAME_IS_KEY != 0;
			}
		}

		Ok((!out.is_empty()).then(|| EncodedChunk {
			data: out.freeze(),
			keyframe,
		}))
	}
}

pub(crate) struct Vp8Decoder {
	ctx: Context,
}

impl Vp8Decoder {
	#[expect(clippy::cast_possible_wrap, reason = "libvpx's ABI version is a small constant")]
	pub fn new() -> Result<Self> {
		let cfg = vpx::vpx_codec_dec_cfg_t {
			threads: default_threads(),
			w: 0,
			h: 0,
		};
		// SAFETY: libvpx initialises the context once `vpx_codec_dec_init_ver` succeeds.
		let ctx = unsafe {
			Context::new(|ctx| {
				vpx::vpx_codec_dec_init_ver(
					ctx,
					vpx::vpx_codec_vp8_dx(),
					&cfg,
					0,
					vpx::VPX_DECODER_ABI_VERSION as c_int,
				)
			})?
		};
		Ok(Self { ctx })
	}
}

impl DecoderBackend for Vp8Decoder {
	fn decode(&mut self, data: &[u8], max_pixels: u64) -> Result<Option<(u32, u32, Vec<u8>)>> {
		// Check before libvpx allocates frame buffers for the declared size.
		if let Some((width, height)) = keyframe_size(data) {
			check_size(width, height, max_pixels)?;
		}
		let len = c_uint::try_from(data.len()).map_err(|_| Error::Codec("VP8 frame too large".into()))?;
		// SAFETY: `data` is valid for `len` bytes, and the returned image is valid (with planes covering
		// its rows at its strides) until the next call into the decoder.
		unsafe {
			let err = vpx::vpx_codec_decode(&mut self.ctx.0, data.as_ptr(), len, ptr::null_mut(), 0);
			self.ctx.check(err)?;

			// VP8 has no frame reordering: at most one picture per input frame.
			let mut iter: vpx::vpx_codec_iter_t = ptr::null();
			let img = vpx::vpx_codec_get_frame(&mut self.ctx.0, &mut iter);
			if img.is_null() {
				return Ok(None);
			}
			let img = &*img;
			if img.fmt != vpx::vpx_img_fmt::VPX_IMG_FMT_I420 {
				return Err(Error::Codec(format!("unexpected VP8 output format {:?}", img.fmt)));
			}

			let planes = Planes::from_raw(
				img.d_w as usize,
				img.d_h as usize,
				std::array::from_fn(|i| img.planes[i].cast_const()),
				[img.stride[0], img.stride[1], img.stride[2]],
			)?;
			Ok(Some(rgb8_picture(&planes, max_pixels)?))
		}
	}
}

fn check(err: vpx::vpx_codec_err_t, ctx: Option<&vpx::vpx_codec_ctx_t>) -> Result<()> {
	if err == vpx::vpx_codec_err_t::VPX_CODEC_OK {
		return Ok(());
	}
	// SAFETY: libvpx returns static NUL-terminated strings, and `ctx` is a live context.
	let message = unsafe {
		let mut message = CStr::from_ptr(vpx::vpx_codec_err_to_string(err))
			.to_string_lossy()
			.into_owned();
		if let Some(ctx) = ctx {
			// libvpx < 1.13 takes `*mut`; newer versions take `*const`, which `*mut` coerces to.
			let detail = vpx::vpx_codec_error_detail(ctx as *const _ as *mut _);
			if !detail.is_null() {
				message = format!("{message}: {}", CStr::from_ptr(detail).to_string_lossy());
			}
		}
		message
	};
	Err(Error::Codec(format!("libvpx: {message}")))
}

#[cfg(test)]
mod tests {
	use super::super::{Codec, tests};

	#[test]
	fn round_trip() {
		tests::round_trip(Codec::Vp8);
	}

	#[test]
	fn keyframes() {
		tests::keyframes(Codec::Vp8);
	}

	#[test]
	fn pixel_formats() {
		tests::pixel_formats(Codec::Vp8);
	}

	#[test]
	fn irregular_timestamps() {
		tests::irregular_timestamps(Codec::Vp8);
	}

	#[test]
	fn forced_keyframe() {
		tests::forced_keyframe(Codec::Vp8);
	}

	#[test]
	fn rejects_invalid_images() {
		tests::rejects_invalid_images(Codec::Vp8);
	}

	#[test]
	fn keyframe_after_clock_jump() {
		tests::keyframe_after_clock_jump(Codec::Vp8);
	}

	#[test]
	fn rejects_oversized_pictures() {
		tests::rejects_oversized_pictures(Codec::Vp8);
	}

	/// A key frame header declaring 16383x16383 fails before libvpx allocates for it.
	#[test]
	fn rejects_oversized_keyframe_headers() {
		let mut data = vec![0x10, 0x02, 0x00, 0x9d, 0x01, 0x2a, 0xff, 0x3f, 0xff, 0x3f];
		data.extend([0u8; 32]);
		assert_eq!(super::super::vp8_header::keyframe_size(&data), Some((16383, 16383)));
		let mut decoder = super::super::Decoder::new(Codec::Vp8).unwrap();
		assert!(matches!(
			decoder.decode(&data, std::time::Duration::ZERO),
			Err(crate::Error::TooLarge {
				width: 16383,
				height: 16383,
				..
			})
		));
	}

	#[test]
	fn keyframe_size_reads_encoded_frames() {
		let mut encoder = super::super::Encoder::new(Default::default()).unwrap();
		let frame = encoder.encode(&tests::test_pattern(64, 48, 0)).unwrap().unwrap();
		assert_eq!(super::super::vp8_header::keyframe_size(&frame.data), Some((64, 48)));
		assert_eq!(super::super::vp8_header::keyframe_size(&[0x11, 0x02, 0x00]), None);
	}

	/// A frame that doesn't match the configured size is refused, not read out of bounds.
	#[test]
	fn encoder_rejects_mismatched_frames() {
		let mut encoder = super::Vp8Encoder::new(&Default::default(), 64, 48).unwrap();
		let frame = crate::convert::I420::from_image(&tests::test_pattern(32, 24, 0)).unwrap();
		assert!(super::super::EncoderBackend::encode(&mut encoder, &frame, 0, false).is_err());
	}

	#[test]
	fn decoder_rejects_garbage() {
		let mut decoder = super::super::Decoder::new(Codec::Vp8).unwrap();
		assert!(decoder.decode(&[0xff; 32], std::time::Duration::ZERO).is_err());
	}
}
