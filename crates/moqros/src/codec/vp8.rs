//! VP8 via libvpx, tuned for real-time streaming.

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::os::raw::c_int;
use std::ptr;

use bytes::{Bytes, BytesMut};
use hang::catalog::VideoCodec;
use vpx_sys as vpx;

use super::{DecoderBackend, EncoderBackend, EncoderSettings};
use crate::convert::{I420, Planes, yuv_to_rgb8};
use crate::{Error, Result};

/// Speed/quality trade-off; negative values select real-time mode in libvpx.
/// -8 is in the range WebRTC uses for desktop-class CPUs.
const CPU_USED: c_int = -8;

pub(crate) struct Vp8Encoder {
	ctx: vpx::vpx_codec_ctx_t,
	width: u32,
	height: u32,
	frame_duration: u64,
}

// SAFETY: a libvpx context may be used from any thread as long as it isn't shared;
// `Vp8Encoder` owns it exclusively and all access goes through `&mut self`.
unsafe impl Send for Vp8Encoder {}

impl Vp8Encoder {
	pub fn new(settings: &EncoderSettings, width: u32, height: u32) -> Result<Self> {
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

			let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::zeroed();
			check(
				vpx::vpx_codec_enc_init_ver(ctx.as_mut_ptr(), iface, &cfg, 0, vpx::VPX_ENCODER_ABI_VERSION as c_int),
				None,
			)?;
			let mut ctx = ctx.assume_init();

			let ret = vpx::vpx_codec_control_(&mut ctx, vpx::vp8e_enc_control_id::VP8E_SET_CPUUSED as c_int, CPU_USED);
			if let Err(err) = check(ret, Some(&ctx)) {
				vpx::vpx_codec_destroy(&mut ctx);
				return Err(err);
			}

			Ok(Self {
				ctx,
				width,
				height,
				frame_duration: (1_000_000.0 / settings.max_fps.max(1.0)) as u64,
			})
		}
	}
}

impl EncoderBackend for Vp8Encoder {
	fn encode(&mut self, frame: &I420, pts: u64, force_keyframe: bool) -> Result<Option<(Bytes, bool)>> {
		debug_assert_eq!((frame.width() as u32, frame.height() as u32), (self.width, self.height));

		let mut out = BytesMut::new();
		let mut keyframe = false;
		unsafe {
			let mut img = MaybeUninit::<vpx::vpx_image_t>::zeroed();
			// libvpx only reads from the wrapped buffer when encoding.
			let wrapped = vpx::vpx_img_wrap(
				img.as_mut_ptr(),
				vpx::vpx_img_fmt::VPX_IMG_FMT_I420,
				self.width,
				self.height,
				1,
				frame.data().as_ptr() as *mut u8,
			);
			if wrapped.is_null() {
				return Err(Error::Codec("vpx_img_wrap failed".into()));
			}

			let flags = if force_keyframe {
				vpx::VPX_EFLAG_FORCE_KF as _
			} else {
				0
			};
			check(
				vpx::vpx_codec_encode(
					&mut self.ctx,
					wrapped,
					pts as i64,
					self.frame_duration as _,
					flags,
					vpx::VPX_DL_REALTIME as _,
				),
				Some(&self.ctx),
			)?;

			let mut iter: vpx::vpx_codec_iter_t = ptr::null();
			loop {
				let pkt = vpx::vpx_codec_get_cx_data(&mut self.ctx, &mut iter);
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

		Ok((!out.is_empty()).then(|| (out.freeze(), keyframe)))
	}

	fn catalog_codec(&self, _keyframe: &[u8]) -> VideoCodec {
		VideoCodec::VP8
	}
}

impl Drop for Vp8Encoder {
	fn drop(&mut self) {
		unsafe {
			vpx::vpx_codec_destroy(&mut self.ctx);
		}
	}
}

pub(crate) struct Vp8Decoder {
	ctx: vpx::vpx_codec_ctx_t,
}

// SAFETY: see `Vp8Encoder`.
unsafe impl Send for Vp8Decoder {}

impl Vp8Decoder {
	pub fn new() -> Result<Self> {
		unsafe {
			let cfg = vpx::vpx_codec_dec_cfg_t {
				threads: std::thread::available_parallelism().map_or(1, |n| n.get().min(4) as u32),
				w: 0,
				h: 0,
			};
			let mut ctx = MaybeUninit::<vpx::vpx_codec_ctx_t>::zeroed();
			check(
				vpx::vpx_codec_dec_init_ver(
					ctx.as_mut_ptr(),
					vpx::vpx_codec_vp8_dx(),
					&cfg,
					0,
					vpx::VPX_DECODER_ABI_VERSION as c_int,
				),
				None,
			)?;
			Ok(Self { ctx: ctx.assume_init() })
		}
	}
}

impl DecoderBackend for Vp8Decoder {
	fn decode(&mut self, data: &[u8]) -> Result<Option<(u32, u32, Vec<u8>)>> {
		unsafe {
			check(
				vpx::vpx_codec_decode(&mut self.ctx, data.as_ptr(), data.len() as _, ptr::null_mut(), 0),
				Some(&self.ctx),
			)?;

			// VP8 has no frame reordering: at most one picture per input frame.
			let mut iter: vpx::vpx_codec_iter_t = ptr::null();
			let img = vpx::vpx_codec_get_frame(&mut self.ctx, &mut iter);
			if img.is_null() {
				return Ok(None);
			}
			let img = &*img;
			if img.fmt != vpx::vpx_img_fmt::VPX_IMG_FMT_I420 {
				return Err(Error::Codec(format!("unexpected VP8 output format {:?}", img.fmt)));
			}

			let (width, height) = (img.d_w as usize, img.d_h as usize);
			let ch = height.div_ceil(2);
			let plane = |i: usize, rows: usize| {
				let stride = img.stride[i] as usize;
				(std::slice::from_raw_parts(img.planes[i], stride * rows), stride)
			};
			let planes = Planes {
				width,
				height,
				y: plane(0, height),
				u: plane(1, ch),
				v: plane(2, ch),
			};
			Ok(Some((width as u32, height as u32, yuv_to_rgb8(&planes))))
		}
	}
}

impl Drop for Vp8Decoder {
	fn drop(&mut self) {
		unsafe {
			vpx::vpx_codec_destroy(&mut self.ctx);
		}
	}
}

fn check(err: vpx::vpx_codec_err_t, ctx: Option<&vpx::vpx_codec_ctx_t>) -> Result<()> {
	if err == vpx::vpx_codec_err_t::VPX_CODEC_OK {
		return Ok(());
	}
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
	fn decoder_rejects_garbage() {
		let mut decoder = super::super::Decoder::new(Codec::Vp8).unwrap();
		assert!(decoder.decode(&[0xff; 32], std::time::Duration::ZERO).is_err());
	}
}
