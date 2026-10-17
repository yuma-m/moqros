//! AV1 via libaom, tuned for real-time streaming.

use std::ffi::CStr;
use std::mem::MaybeUninit;
use std::os::raw::{c_int, c_uint};
use std::ptr;

use aom_sys as aom;
use bytes::BytesMut;

use super::{
	DecoderBackend, EncodedChunk, EncoderBackend, EncoderSettings, check_size, default_threads, obu, rgb8_picture,
};
use crate::convert::{I420, Planes};
use crate::{Error, Result};

/// Speed/quality trade-off of libaom's real-time mode; 9 is fast enough for live video on
/// desktop-class CPUs and accepted by every libaom 3.x.
const CPU_USED: c_int = 9;

pub(crate) struct Av1Encoder {
	ctx: aom::aom_codec_ctx_t,
	width: u32,
	height: u32,
	frame_duration: u64,
}

// SAFETY: a libaom context may be used from any thread as long as it isn't shared;
// `Av1Encoder` owns it exclusively and all access goes through `&mut self`.
unsafe impl Send for Av1Encoder {}

impl Av1Encoder {
	pub fn new(settings: &EncoderSettings, width: u32, height: u32) -> Result<Self> {
		unsafe {
			let iface = aom::aom_codec_av1_cx();
			let mut cfg = MaybeUninit::<aom::aom_codec_enc_cfg_t>::zeroed();
			check(
				aom::aom_codec_enc_config_default(iface, cfg.as_mut_ptr(), aom::AOM_USAGE_REALTIME as c_uint),
				None,
			)?;
			let mut cfg = cfg.assume_init();

			let threads = settings.threads();
			cfg.g_w = width;
			cfg.g_h = height;
			// Timestamps are passed in microseconds.
			cfg.g_timebase = aom::aom_rational { num: 1, den: 1_000_000 };
			cfg.g_threads = threads;
			cfg.g_lag_in_frames = 0;
			cfg.g_error_resilient = 0;
			cfg.rc_end_usage = aom::aom_rc_mode::AOM_CBR;
			cfg.rc_target_bitrate = (settings.bitrate / 1000).max(1);
			cfg.rc_min_quantizer = 4;
			cfg.rc_max_quantizer = 56;
			cfg.rc_dropframe_thresh = 0;
			cfg.kf_mode = aom::aom_kf_mode::AOM_KF_AUTO;
			cfg.kf_max_dist = settings.gop_frames();

			let mut ctx = MaybeUninit::<aom::aom_codec_ctx_t>::zeroed();
			check(
				aom::aom_codec_enc_init_ver(ctx.as_mut_ptr(), iface, &cfg, 0, aom::AOM_ENCODER_ABI_VERSION as c_int),
				None,
			)?;
			let mut ctx = ctx.assume_init();

			// Tile columns (log2) let row-based multithreading spread one frame over the threads.
			let tile_columns = threads.max(1).ilog2().min(2) as c_int;
			for (id, value) in [
				(aom::aome_enc_control_id::AOME_SET_CPUUSED as c_int, CPU_USED),
				(aom::aome_enc_control_id::AV1E_SET_ROW_MT as c_int, 1),
				(aom::aome_enc_control_id::AV1E_SET_TILE_COLUMNS as c_int, tile_columns),
			] {
				if let Err(err) = check(aom::aom_codec_control(&mut ctx, id, value), Some(&ctx)) {
					aom::aom_codec_destroy(&mut ctx);
					return Err(err);
				}
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

impl EncoderBackend for Av1Encoder {
	fn encode(&mut self, frame: &I420, pts: u64, force_keyframe: bool) -> Result<Option<EncodedChunk>> {
		debug_assert_eq!((frame.width() as u32, frame.height() as u32), (self.width, self.height));

		let mut out = BytesMut::new();
		let mut keyframe = false;
		unsafe {
			let mut img = MaybeUninit::<aom::aom_image_t>::zeroed();
			// libaom only reads from the wrapped buffer when encoding.
			let wrapped = aom::aom_img_wrap(
				img.as_mut_ptr(),
				aom::aom_img_fmt::AOM_IMG_FMT_I420,
				self.width,
				self.height,
				1,
				frame.data().as_ptr() as *mut u8,
			);
			if wrapped.is_null() {
				return Err(Error::Codec("aom_img_wrap failed".into()));
			}

			let flags = if force_keyframe {
				aom::AOM_EFLAG_FORCE_KF as aom::aom_enc_frame_flags_t
			} else {
				0
			};
			check(
				aom::aom_codec_encode(&mut self.ctx, wrapped, pts as i64, self.frame_duration as _, flags),
				Some(&self.ctx),
			)?;

			let mut iter: aom::aom_codec_iter_t = ptr::null();
			loop {
				let pkt = aom::aom_codec_get_cx_data(&mut self.ctx, &mut iter);
				if pkt.is_null() {
					break;
				}
				if (*pkt).kind != aom::aom_codec_cx_pkt_kind::AOM_CODEC_CX_FRAME_PKT {
					continue;
				}
				let f = (*pkt).data.frame;
				out.extend_from_slice(std::slice::from_raw_parts(f.buf as *const u8, f.sz));
				keyframe |= f.flags & aom::AOM_FRAME_IS_KEY != 0;
			}
		}

		Ok((!out.is_empty()).then(|| EncodedChunk {
			data: out.freeze(),
			keyframe,
		}))
	}
}

impl Drop for Av1Encoder {
	fn drop(&mut self) {
		unsafe {
			aom::aom_codec_destroy(&mut self.ctx);
		}
	}
}

pub(crate) struct Av1Decoder {
	ctx: aom::aom_codec_ctx_t,
}

// SAFETY: see `Av1Encoder`.
unsafe impl Send for Av1Decoder {}

impl Av1Decoder {
	pub fn new() -> Result<Self> {
		unsafe {
			let cfg = aom::aom_codec_dec_cfg_t {
				threads: default_threads(),
				w: 0,
				h: 0,
				// Output 8-bit streams as 8-bit images even when libaom is built for high bit depth.
				allow_lowbitdepth: 1,
			};
			let mut ctx = MaybeUninit::<aom::aom_codec_ctx_t>::zeroed();
			check(
				aom::aom_codec_dec_init_ver(
					ctx.as_mut_ptr(),
					aom::aom_codec_av1_dx(),
					&cfg,
					0,
					aom::AOM_DECODER_ABI_VERSION as c_int,
				),
				None,
			)?;
			Ok(Self { ctx: ctx.assume_init() })
		}
	}
}

impl DecoderBackend for Av1Decoder {
	fn decode(&mut self, data: &[u8], max_pixels: u64) -> Result<Option<(u32, u32, Vec<u8>)>> {
		// Check before libaom allocates frame buffers for the declared size. libaom applies every
		// sequence header in the temporal unit, so check each one, and refuse one that can't be
		// read rather than let libaom size frames from it unchecked.
		for header in obu::sequence_headers(data) {
			let header = header.ok_or_else(|| Error::Codec("malformed AV1 sequence header".into()))?;
			check_size(header.max_width, header.max_height, max_pixels)?;
		}
		unsafe {
			check(
				aom::aom_codec_decode(&mut self.ctx, data.as_ptr(), data.len(), ptr::null_mut()),
				Some(&self.ctx),
			)?;

			// A temporal unit shows at most one frame.
			let mut iter: aom::aom_codec_iter_t = ptr::null();
			let img = aom::aom_codec_get_frame(&mut self.ctx, &mut iter);
			if img.is_null() {
				return Ok(None);
			}
			let img = &*img;
			if img.fmt != aom::aom_img_fmt::AOM_IMG_FMT_I420 {
				return Err(Error::Codec(format!(
					"unsupported AV1 output format {:?} (only 8-bit 4:2:0 is supported)",
					img.fmt
				)));
			}

			let planes = Planes::from_raw(
				img.d_w as usize,
				img.d_h as usize,
				std::array::from_fn(|i| img.planes[i].cast_const()),
				std::array::from_fn(|i| img.stride[i] as usize),
			);
			Ok(Some(rgb8_picture(&planes, max_pixels)?))
		}
	}
}

impl Drop for Av1Decoder {
	fn drop(&mut self) {
		unsafe {
			aom::aom_codec_destroy(&mut self.ctx);
		}
	}
}

fn check(err: aom::aom_codec_err_t::Type, ctx: Option<&aom::aom_codec_ctx_t>) -> Result<()> {
	if err == aom::aom_codec_err_t::AOM_CODEC_OK {
		return Ok(());
	}
	let message = unsafe {
		let mut message = CStr::from_ptr(aom::aom_codec_err_to_string(err))
			.to_string_lossy()
			.into_owned();
		if let Some(ctx) = ctx {
			// libaom < 3.4 takes `*mut`; newer versions take `*const`, which `*mut` coerces to.
			let detail = aom::aom_codec_error_detail(ctx as *const _ as *mut _);
			if !detail.is_null() {
				message = format!("{message}: {}", CStr::from_ptr(detail).to_string_lossy());
			}
		}
		message
	};
	Err(Error::Codec(format!("libaom: {message}")))
}

#[cfg(test)]
mod tests {
	use super::super::obu::tests::{obu, sequence_header_payload};
	use super::super::{Codec, Decoder, Encoder, EncoderSettings, tests};

	#[test]
	fn round_trip() {
		tests::round_trip(Codec::Av1);
	}

	#[test]
	fn keyframes() {
		tests::keyframes(Codec::Av1);
	}

	#[test]
	fn pixel_formats() {
		tests::pixel_formats(Codec::Av1);
	}

	#[test]
	fn irregular_timestamps() {
		tests::irregular_timestamps(Codec::Av1);
	}

	#[test]
	fn forced_keyframe() {
		tests::forced_keyframe(Codec::Av1);
	}

	#[test]
	fn rejects_invalid_images() {
		tests::rejects_invalid_images(Codec::Av1);
	}

	#[test]
	fn keyframe_after_clock_jump() {
		tests::keyframe_after_clock_jump(Codec::Av1);
	}

	#[test]
	fn rejects_oversized_pictures() {
		tests::rejects_oversized_pictures(Codec::Av1);
	}

	/// A sequence header declaring 65536x65536 fails before libaom allocates for it.
	#[test]
	fn rejects_oversized_sequence_headers() {
		let mut data = obu(2, &[]);
		data.extend(obu(1, &sequence_header_payload(0, 8, 65536, 65536)));
		let mut decoder = Decoder::new(Codec::Av1).unwrap();
		assert!(matches!(
			decoder.decode(&data, std::time::Duration::ZERO),
			Err(crate::Error::TooLarge {
				width: 65536,
				height: 65536,
				..
			})
		));
	}

	/// A later sequence header in the same temporal unit is checked too, even past zero padding.
	#[test]
	fn rejects_oversized_later_sequence_headers() {
		let small = obu(1, &sequence_header_payload(0, 8, 64, 48));
		let large = obu(1, &sequence_header_payload(0, 8, 65536, 65536));
		for padding in [0, 2] {
			let mut data = obu(2, &[]);
			data.extend(&small);
			data.resize(data.len() + padding, 0);
			data.extend(&large);
			let mut decoder = Decoder::new(Codec::Av1).unwrap();
			assert!(matches!(
				decoder.decode(&data, std::time::Duration::ZERO),
				Err(crate::Error::TooLarge {
					width: 65536,
					height: 65536,
					..
				})
			));
		}
	}

	#[test]
	fn catalog_codec_from_sequence_header() {
		let mut encoder = Encoder::new(EncoderSettings {
			codec: Codec::Av1,
			..Default::default()
		})
		.unwrap();
		let frame = encoder.encode(&tests::test_pattern(64, 48, 0)).unwrap().unwrap();
		let header = super::obu::sequence_header(&frame.data).expect("keyframes carry a sequence header");
		assert_eq!((header.max_width, header.max_height, header.bit_depth), (64, 48, 8));
		assert_eq!(frame.codec.to_string(), "av01.0.00M.08");
		// libaom picks the level for the picture size: 3.1 for 720p.
		let frame = encoder.encode(&tests::test_pattern(1280, 720, 1)).unwrap().unwrap();
		assert_eq!(frame.codec.to_string(), "av01.0.05M.08");
	}

	#[test]
	fn decoder_rejects_garbage() {
		let mut decoder = Decoder::new(Codec::Av1).unwrap();
		assert!(decoder.decode(&[0xff; 32], std::time::Duration::ZERO).is_err());
	}
}
