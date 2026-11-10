//! Color conversion between [`Image`] pixel formats and planar I420 (YUV 4:2:0).
//!
//! Uses BT.601 limited range, which is what VP8/H.264 decoders (and browsers) assume when the
//! bitstream carries no color description. RGB conversions run on yuvutils-rs's SIMD kernels.

use yuvutils_rs::{BufferStoreMut, YuvConversionMode, YuvPlanarImage, YuvPlanarImageMut, YuvRange, YuvStandardMatrix};

use crate::{Error, Image, PixelFormat};

/// A picture dimension or stride as yuvutils-rs takes it. Image dimensions come from `u32`.
#[expect(clippy::cast_possible_truncation, reason = "image dimensions come from u32")]
fn dim(n: usize) -> u32 {
	n as u32
}

/// A tightly packed I420 frame with even dimensions: the Y plane, then U, then V,
/// in one contiguous buffer (the layout `vpx_img_wrap` expects). The default is an empty 0x0
/// frame, to [fill](I420::fill_from_image).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct I420 {
	width: usize,
	height: usize,
	data: Vec<u8>,
}

impl I420 {
	/// Width in pixels.
	pub fn width(&self) -> usize {
		self.width
	}

	/// Height in pixels.
	pub fn height(&self) -> usize {
		self.height
	}

	/// The whole contiguous Y+U+V buffer.
	pub fn data(&self) -> &[u8] {
		&self.data
	}

	/// The luma plane, `width * height` bytes.
	pub fn y(&self) -> &[u8] {
		&self.data[..self.luma_len()]
	}

	/// The Cb plane, a quarter of the luma plane.
	pub fn u(&self) -> &[u8] {
		let (l, c) = (self.luma_len(), self.luma_len() / 4);
		&self.data[l..l + c]
	}

	/// The Cr plane, a quarter of the luma plane.
	pub fn v(&self) -> &[u8] {
		let (l, c) = (self.luma_len(), self.luma_len() / 4);
		&self.data[l + c..]
	}

	/// Borrow as generic [`Planes`].
	pub fn planes(&self) -> Planes<'_> {
		Planes {
			width: self.width,
			height: self.height,
			y: (self.y(), self.width),
			u: (self.u(), self.width / 2),
			v: (self.v(), self.width / 2),
		}
	}

	fn luma_len(&self) -> usize {
		self.width * self.height
	}

	/// Mutable (Y, U, V) planes.
	fn planes_mut(&mut self) -> (&mut [u8], &mut [u8], &mut [u8]) {
		let l = self.luma_len();
		let (y, uv) = self.data.split_at_mut(l);
		let (u, v) = uv.split_at_mut(l / 4);
		(y, u, v)
	}

	/// A black frame of `width`x`height` (both even).
	#[cfg(feature = "hw")]
	pub fn black(width: usize, height: usize) -> Self {
		let luma = width * height;
		let mut data = vec![128u8; luma + luma / 2];
		data[..luma].fill(16);
		Self { width, height, data }
	}

	/// This picture scaled down to `width`x`height` (both even, and at most this picture's size)
	/// with a box filter: each output pixel is the mean of the input pixels it covers.
	pub(crate) fn downscale(&self, width: usize, height: usize) -> Self {
		debug_assert!(
			width.is_multiple_of(2) && height.is_multiple_of(2) && width <= self.width && height <= self.height
		);
		let mut out = Self {
			width,
			height,
			data: vec![0; width * height * 3 / 2],
		};
		let src = self.planes();
		let (y, u, v) = out.planes_mut();
		scale_plane(src.y.0, self.width, self.height, y, width, height);
		scale_plane(src.u.0, self.width / 2, self.height / 2, u, width / 2, height / 2);
		scale_plane(src.v.0, self.width / 2, self.height / 2, v, width / 2, height / 2);
		out
	}

	/// Convert any supported [`Image`] to I420.
	///
	/// 4:2:0 video needs even dimensions, so an odd trailing row or column is cropped.
	pub fn from_image(image: &Image) -> Result<Self, Error> {
		let mut out = Self::default();
		out.fill_from_image(image)?;
		Ok(out)
	}

	/// Convert `image` into this frame, reusing its buffer; see [`I420::from_image`]. After an
	/// error the frame holds no meaningful picture.
	pub fn fill_from_image(&mut self, image: &Image) -> Result<(), Error> {
		image.validate()?;
		let width = image.width as usize & !1;
		let height = image.height as usize & !1;
		if width == 0 || height == 0 {
			return Err(Error::InvalidImage(format!(
				"{}x{} is too small to encode",
				image.width, image.height
			)));
		}

		self.width = width;
		self.height = height;
		// Every byte is written below, so only growth needs initializing.
		self.data.resize(width * height * 3 / 2, 0);
		match image.format {
			PixelFormat::Rgb8 | PixelFormat::Bgr8 | PixelFormat::Rgba8 | PixelFormat::Bgra8 => self.fill_rgb(image)?,
			PixelFormat::Mono8 => self.fill_mono(image),
			// Offsets of (Y0, U, Y1, V) inside each 4-byte macropixel.
			PixelFormat::Yuyv => self.fill_packed_422(image, [0, 1, 2, 3]),
			PixelFormat::Uyvy => self.fill_packed_422(image, [1, 0, 3, 2]),
		}
		Ok(())
	}

	/// Fill from packed RGB, RGBA, BGR or BGRA.
	fn fill_rgb(&mut self, image: &Image) -> Result<(), Error> {
		let (width, height) = (self.width, self.height);
		let (y, u, v) = self.planes_mut();
		let mut planar = YuvPlanarImageMut {
			y_plane: BufferStoreMut::Borrowed(y),
			y_stride: dim(width),
			u_plane: BufferStoreMut::Borrowed(u),
			u_stride: dim(width / 2),
			v_plane: BufferStoreMut::Borrowed(v),
			v_stride: dim(width / 2),
			width: dim(width),
			height: dim(height),
		};
		let convert = match image.format {
			PixelFormat::Rgb8 => yuvutils_rs::rgb_to_yuv420,
			PixelFormat::Bgr8 => yuvutils_rs::bgr_to_yuv420,
			PixelFormat::Rgba8 => yuvutils_rs::rgba_to_yuv420,
			PixelFormat::Bgra8 => yuvutils_rs::bgra_to_yuv420,
			other => unreachable!("{other} isn't packed RGB"),
		};
		// Only the rows that are kept: an odd last one is cropped.
		let rows = &image.data[..image.step as usize * height];
		convert(
			&mut planar,
			rows,
			image.step,
			YuvRange::Limited,
			YuvStandardMatrix::Bt601,
			YuvConversionMode::Balanced,
		)
		.map_err(|err| Error::InvalidImage(format!("color conversion: {err}")))
	}

	#[expect(clippy::cast_possible_truncation, reason = "the result is in 0..=255")]
	fn fill_mono(&mut self, image: &Image) {
		let width = self.width;
		let (y_plane, u_plane, v_plane) = self.planes_mut();
		for (y, dst) in y_plane.chunks_exact_mut(width).enumerate() {
			for (d, &s) in dst.iter_mut().zip(&image.row(y)[..width]) {
				*d = (16 + ((u32::from(s) * 219 + 127) / 255)) as u8;
			}
		}
		u_plane.fill(128);
		v_plane.fill(128);
	}

	#[expect(clippy::cast_possible_truncation, reason = "the result is in 0..=255")]
	fn fill_packed_422(&mut self, image: &Image, [y0, u, y1, v]: [usize; 4]) {
		let width = self.width;
		let cw = width / 2;
		let (y_plane, u_plane, v_plane) = self.planes_mut();
		let rows = y_plane
			.chunks_exact_mut(width * 2)
			.zip(u_plane.chunks_exact_mut(cw))
			.zip(v_plane.chunks_exact_mut(cw));
		for (cy, ((y_rows, u_row), v_row)) in rows.enumerate() {
			let (y_top, y_bottom) = y_rows.split_at_mut(width);
			let src0 = &image.row(cy * 2)[..width * 2];
			let src1 = &image.row(cy * 2 + 1)[..width * 2];
			let blocks = src0
				.as_chunks::<4>()
				.0
				.iter()
				.zip(src1.as_chunks::<4>().0.iter())
				.zip(
					y_top
						.as_chunks_mut::<2>()
						.0
						.iter_mut()
						.zip(y_bottom.as_chunks_mut::<2>().0.iter_mut()),
				)
				.zip(u_row.iter_mut().zip(v_row.iter_mut()));
			for (((top, bottom), (yt, yb)), (du, dv)) in blocks {
				yt[0] = top[y0];
				yt[1] = top[y1];
				yb[0] = bottom[y0];
				yb[1] = bottom[y1];
				*du = (u32::from(top[u]) + u32::from(bottom[u])).div_ceil(2) as u8;
				*dv = (u32::from(top[v]) + u32::from(bottom[v])).div_ceil(2) as u8;
			}
		}
	}
}

/// Box-filter the tightly packed `sw`x`sh` plane `src` into the `dw`x`dh` plane `dst`.
#[expect(clippy::cast_possible_truncation, reason = "a mean of bytes fits a byte")]
fn scale_plane(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
	// The input columns each output column covers, the same for every row.
	let columns: Vec<(usize, usize)> = (0..dw)
		.map(|dx| {
			let x0 = dx * sw / dw;
			(x0, ((dx + 1) * sw / dw).max(x0 + 1))
		})
		.collect();
	for (dy, row) in dst.chunks_exact_mut(dw).enumerate() {
		let y0 = dy * sh / dh;
		let y1 = ((dy + 1) * sh / dh).max(y0 + 1);
		for (out, &(x0, x1)) in row.iter_mut().zip(&columns) {
			let sum: usize = (y0..y1)
				.map(|y| {
					src[y * sw + x0..y * sw + x1]
						.iter()
						.map(|&p| usize::from(p))
						.sum::<usize>()
				})
				.sum();
			let count = (y1 - y0) * (x1 - x0);
			*out = ((sum + count / 2) / count) as u8;
		}
	}
}

/// Borrowed 4:2:0 planes with arbitrary strides, e.g. a decoder's output picture.
#[derive(Clone, Copy, Debug)]
pub struct Planes<'a> {
	/// Width in pixels.
	pub width: usize,
	/// Height in pixels.
	pub height: usize,
	/// Luma plane and its stride in bytes.
	pub y: (&'a [u8], usize),
	/// Cb plane and its stride in bytes.
	pub u: (&'a [u8], usize),
	/// Cr plane and its stride in bytes.
	pub v: (&'a [u8], usize),
}

impl<'a> Planes<'a> {
	/// Planes of a `width`x`height` picture in a decoder-owned buffer.
	///
	/// Fails for a null plane, or a stride that is negative (a flipped image) or narrower than
	/// the plane, rather than read outside it.
	///
	/// # Safety
	///
	/// Each non-null pointer with a stride of at least its plane's width (`width` for luma, half
	/// of it rounded up for chroma) must be valid for reads of its rows (`height` for luma, half
	/// of it rounded up for chroma) for `'a`: the stride times all rows but the last, plus the
	/// plane's width.
	pub(crate) unsafe fn from_raw(
		width: usize,
		height: usize,
		planes: [*const u8; 3],
		strides: [i32; 3],
	) -> Result<Self, Error> {
		let (chroma_width, chroma_height) = (width.div_ceil(2), height.div_ceil(2));
		let dims = [
			(width, height),
			(chroma_width, chroma_height),
			(chroma_width, chroma_height),
		];
		let plane = |i: usize| {
			let (plane_width, rows) = dims[i];
			let stride = usize::try_from(strides[i])
				.ok()
				.filter(|&stride| stride >= plane_width)
				.ok_or_else(|| Error::Codec(format!("invalid stride {} for a {width}px wide picture", strides[i])))?;
			if rows == 0 || plane_width == 0 {
				return Ok((&[][..], stride));
			}
			if planes[i].is_null() {
				return Err(Error::Codec("decoder returned a null plane".into()));
			}
			let len = stride
				.checked_mul(rows - 1)
				.and_then(|n| n.checked_add(plane_width))
				.filter(|&len| isize::try_from(len).is_ok())
				.ok_or_else(|| Error::Codec(format!("{width}x{height} picture overflows memory")))?;
			// SAFETY: the pointer is non-null and its stride is at least its plane's width, so
			// the caller vouches that it is valid for `len` bytes.
			Ok((unsafe { std::slice::from_raw_parts(planes[i], len) }, stride))
		};
		Ok(Self {
			width,
			height,
			y: plane(0)?,
			u: plane(1)?,
			v: plane(2)?,
		})
	}
}

/// Convert planar 4:2:0 into packed `rgb8`.
///
/// # Panics
///
/// If the planes are shorter than their strides and the picture size say.
pub fn yuv_to_rgb8(src: &Planes<'_>) -> Vec<u8> {
	yuv_to_packed(src, PixelFormat::Rgb8).expect("rgb8 is a packed format")
}

/// Convert planar 4:2:0 into packed `format`: `rgb8`, `bgr8`, `rgba8` or `bgra8` (opaque), or
/// `mono8` (the luma, in full range). Fails with [`Error::InvalidSettings`] for the 4:2:2 formats.
///
/// # Panics
///
/// If the planes are shorter than their strides and the picture size say.
pub fn yuv_to_packed(src: &Planes<'_>, format: PixelFormat) -> Result<Vec<u8>, Error> {
	type Kernel =
		fn(&YuvPlanarImage<'_, u8>, &mut [u8], u32, YuvRange, YuvStandardMatrix) -> Result<(), yuvutils_rs::YuvError>;
	// The kernel, and where R, G, B and alpha go in a pixel.
	let (kernel, order, alpha): (Kernel, [usize; 3], Option<usize>) = match format {
		PixelFormat::Rgb8 => (yuvutils_rs::yuv420_to_rgb, [0, 1, 2], None),
		PixelFormat::Bgr8 => (yuvutils_rs::yuv420_to_bgr, [2, 1, 0], None),
		PixelFormat::Rgba8 => (yuvutils_rs::yuv420_to_rgba, [0, 1, 2], Some(3)),
		PixelFormat::Bgra8 => (yuvutils_rs::yuv420_to_bgra, [2, 1, 0], Some(3)),
		PixelFormat::Mono8 => return Ok(luma_to_mono(src)),
		PixelFormat::Uyvy | PixelFormat::Yuyv => {
			return Err(Error::InvalidSettings(format!(
				"can't convert decoded pictures to {format}"
			)));
		}
	};
	let bpp = format.bytes_per_pixel();
	let (width, height) = (src.width, src.height);
	let mut out = vec![0u8; width * height * bpp];
	if width == 0 || height == 0 {
		return Ok(out);
	}
	// yuvutils-rs 0.8.3 pairs the last row of an odd-height picture with the wrong chroma row,
	// so that row is converted here.
	let even = height & !1;
	if even > 0 {
		let planar = YuvPlanarImage {
			y_plane: src.y.0,
			y_stride: dim(src.y.1),
			u_plane: src.u.0,
			u_stride: dim(src.u.1),
			v_plane: src.v.0,
			v_stride: dim(src.v.1),
			width: dim(width),
			height: dim(even),
		};
		kernel(
			&planar,
			&mut out[..width * even * bpp],
			dim(width * bpp),
			YuvRange::Limited,
			YuvStandardMatrix::Bt601,
		)
		.expect("the planes cover the picture");
	}
	if even < height {
		let y_row = &src.y.0[even * src.y.1..];
		let u_row = &src.u.0[(even / 2) * src.u.1..];
		let v_row = &src.v.0[(even / 2) * src.v.1..];
		for (x, pixel) in out[even * width * bpp..].chunks_exact_mut(bpp).enumerate() {
			let rgb = yuv_to_rgb(y_row[x], u_row[x / 2], v_row[x / 2]);
			for (channel, &index) in order.iter().enumerate() {
				pixel[index] = rgb[channel];
			}
			if let Some(index) = alpha {
				pixel[index] = 255;
			}
		}
	}
	Ok(out)
}

/// The luma plane as packed full-range `mono8`, inverting [`I420::from_image`]'s mapping.
fn luma_to_mono(src: &Planes<'_>) -> Vec<u8> {
	let (width, height) = (src.width, src.height);
	let mut out = vec![0u8; width * height];
	if width == 0 {
		return out;
	}
	let (plane, stride) = src.y;
	for (row, dst) in out.chunks_exact_mut(width).enumerate() {
		for (d, &y) in dst.iter_mut().zip(&plane[row * stride..row * stride + width]) {
			*d = u8::try_from(((u32::from(y.clamp(16, 235)) - 16) * 255 + 109) / 219).unwrap_or(u8::MAX);
		}
	}
	out
}

#[expect(clippy::cast_sign_loss, reason = "the result is in 0..=255")]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
	let c = i32::from(y) - 16;
	let d = i32::from(u) - 128;
	let e = i32::from(v) - 128;
	let clamp = |x: i32| x.clamp(0, 255) as u8;
	[
		clamp((298 * c + 409 * e + 128) >> 8),
		clamp((298 * c - 100 * d - 208 * e + 128) >> 8),
		clamp((298 * c + 516 * d + 128) >> 8),
	]
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use super::*;

	fn max_diff(a: &[u8], b: &[u8]) -> u8 {
		a.iter().zip(b).map(|(a, b)| a.abs_diff(*b)).max().unwrap_or(0)
	}

	/// A smooth gradient survives RGB -> I420 -> RGB with small error.
	#[test]
	#[expect(clippy::cast_possible_truncation, reason = "the gradient fits in a byte")]
	fn rgb_round_trip() {
		let (w, h) = (16u32, 8u32);
		let rgb: Vec<u8> = (0..h)
			.flat_map(|y| (0..w).flat_map(move |x| [(x * 16) as u8, (y * 32) as u8, 128]))
			.collect();
		let image = Image::new(w, h, PixelFormat::Rgb8, rgb.clone(), Duration::ZERO).unwrap();
		let back = yuv_to_rgb8(&I420::from_image(&image).unwrap().planes());
		assert!(max_diff(&rgb, &back) <= 24, "diff {}", max_diff(&rgb, &back));
	}

	#[test]
	fn channel_orders_agree() {
		let rgb = [10u8, 200, 60].repeat(4);
		let bgr = [60u8, 200, 10].repeat(4);
		let bgra = [60u8, 200, 10, 255].repeat(4);
		let a = I420::from_image(&Image::new(2, 2, PixelFormat::Rgb8, rgb, Duration::ZERO).unwrap()).unwrap();
		let b = I420::from_image(&Image::new(2, 2, PixelFormat::Bgr8, bgr, Duration::ZERO).unwrap()).unwrap();
		let c = I420::from_image(&Image::new(2, 2, PixelFormat::Bgra8, bgra, Duration::ZERO).unwrap()).unwrap();
		assert_eq!(a, b);
		assert_eq!(a, c);
	}

	#[test]
	fn packed_422_orders_agree() {
		let yuyv = vec![50u8, 100, 60, 150, 70, 110, 80, 160];
		let uyvy = vec![100u8, 50, 150, 60, 110, 70, 160, 80];
		let a = I420::from_image(&Image::new(2, 2, PixelFormat::Yuyv, yuyv, Duration::ZERO).unwrap()).unwrap();
		let b = I420::from_image(&Image::new(2, 2, PixelFormat::Uyvy, uyvy, Duration::ZERO).unwrap()).unwrap();
		assert_eq!(a, b);
		assert_eq!(a.y(), &[50, 60, 70, 80]);
		assert_eq!(a.u(), &[105]);
		assert_eq!(a.v(), &[155]);
	}

	#[test]
	fn crops_odd_dimensions_and_honors_step() {
		// 3x3 mono with 2 bytes of row padding.
		let data = vec![255u8; 5 * 3];
		let image = Image::with_step(3, 3, PixelFormat::Mono8, 5, data, Duration::ZERO).unwrap();
		let i420 = I420::from_image(&image).unwrap();
		assert_eq!((i420.width(), i420.height()), (2, 2));
		assert!(i420.y().iter().all(|&y| y == 235));
	}

	fn i420(width: u32, height: u32, format: PixelFormat, data: Vec<u8>) -> I420 {
		I420::from_image(&Image::new(width, height, format, data, Duration::ZERO).unwrap()).unwrap()
	}

	/// BT.601 limited-range values (within 1 of the exact ones, from the integer math).
	#[test]
	fn converts_reference_colors() {
		for (rgb, yuv) in [
			([0u8, 0, 0], [16u8, 128, 128]),
			([255, 255, 255], [235, 128, 128]),
			([255, 0, 0], [82, 90, 240]),
			([0, 255, 0], [144, 54, 34]),
			([0, 0, 255], [41, 240, 110]),
		] {
			let frame = i420(2, 2, PixelFormat::Rgb8, rgb.repeat(4));
			assert!(max_diff(frame.y(), &[yuv[0]; 4]) <= 1, "{rgb:?}: {:?}", frame.y());
			assert!(max_diff(frame.u(), &[yuv[1]]) <= 1, "{rgb:?}: {:?}", frame.u());
			assert!(max_diff(frame.v(), &[yuv[2]]) <= 1, "{rgb:?}: {:?}", frame.v());
		}
	}

	#[test]
	fn averages_chroma_over_2x2_blocks() {
		// Left column black, right column white.
		let rgb = [[0u8; 3], [255; 3], [0; 3], [255; 3]].concat();
		let frame = i420(2, 2, PixelFormat::Rgb8, rgb);
		assert_eq!(frame.y(), &[16, 235, 16, 235]);
		assert_eq!((frame.u(), frame.v()), (&[128][..], &[128][..]));
	}

	#[test]
	fn rgba_ignores_alpha() {
		let rgb = i420(2, 2, PixelFormat::Rgb8, [10u8, 200, 60].repeat(4));
		let rgba = i420(2, 2, PixelFormat::Rgba8, [10u8, 200, 60, 0].repeat(4));
		assert_eq!(rgb, rgba);
	}

	#[test]
	fn mono_maps_to_limited_range_gray() {
		let frame = i420(4, 2, PixelFormat::Mono8, vec![0, 128, 255, 64, 0, 128, 255, 64]);
		assert_eq!(frame.y(), &[16, 126, 235, 71, 16, 126, 235, 71]);
		assert!(frame.u().iter().chain(frame.v()).all(|&c| c == 128));
	}

	#[test]
	fn rejects_images_too_small_to_encode() {
		for (width, height) in [(1, 1), (1, 4), (4, 1), (0, 0)] {
			let image = Image::new(width, height, PixelFormat::Mono8, vec![0u8; 4], Duration::ZERO).unwrap();
			assert!(
				matches!(I420::from_image(&image), Err(Error::InvalidImage(_))),
				"{width}x{height}"
			);
		}
	}

	#[test]
	fn rejects_invalid_images() {
		let mut image = Image::new(2, 2, PixelFormat::Rgb8, vec![0u8; 12], Duration::ZERO).unwrap();
		image.width = 4;
		assert!(matches!(I420::from_image(&image), Err(Error::InvalidImage(_))));
	}

	#[test]
	fn plane_layout() {
		let frame = i420(6, 4, PixelFormat::Mono8, vec![0u8; 24]);
		assert_eq!(frame.data().len(), 36);
		assert_eq!((frame.y().len(), frame.u().len(), frame.v().len()), (24, 6, 6));
		let planes = frame.planes();
		assert_eq!((planes.width, planes.height), (6, 4));
		assert_eq!((planes.y.1, planes.u.1, planes.v.1), (6, 3, 3));
	}

	#[test]
	fn yuv_to_rgb_reference_colors_and_clamping() {
		let rgb = |y: u8, u: u8, v: u8| {
			yuv_to_rgb8(&Planes {
				width: 1,
				height: 1,
				y: (&[y], 1),
				u: (&[u], 1),
				v: (&[v], 1),
			})
		};
		assert_eq!(rgb(16, 128, 128), [0, 0, 0]);
		assert_eq!(rgb(235, 128, 128), [255, 255, 255]);
		assert_eq!(rgb(0, 128, 128), [0, 0, 0]);
		assert_eq!(rgb(255, 128, 128), [255, 255, 255]);
		assert!(max_diff(&rgb(82, 90, 240), &[255, 0, 0]) <= 1);
	}

	/// Decoders hand out planes with padded strides and, for odd sizes, rounded-up chroma.
	#[test]
	fn yuv_to_rgb_honors_strides_and_odd_sizes() {
		let (width, height) = (3, 3);
		let tight = I420 {
			width: 4,
			height: 4,
			data: (0..24).map(|i| 16 + i * 8).collect(),
		};
		let pad = |plane: &[u8], width: usize, stride: usize| -> Vec<u8> {
			plane
				.chunks(width)
				.flat_map(|row| [row, &vec![0xEE; stride - width]].concat())
				.collect()
		};
		let y = pad(tight.y(), 4, 7);
		let u = pad(tight.u(), 2, 5);
		let v = pad(tight.v(), 2, 5);
		let padded = yuv_to_rgb8(&Planes {
			width,
			height,
			y: (&y, 7),
			u: (&u, 5),
			v: (&v, 5),
		});
		let full = yuv_to_rgb8(&tight.planes());
		assert_eq!(padded.len(), width * height * 3);
		for row in 0..height {
			// The last row of an odd-height picture is converted apart, rounding a little differently.
			let (padded, full) = (&padded[row * 9..row * 9 + 9], &full[row * 12..row * 12 + 9]);
			assert!(max_diff(padded, full) <= 2, "row {row}: {padded:?} vs {full:?}");
		}
	}

	/// Decoder planes are bounded by their rows, not by stride times rows.
	#[test]
	fn raw_planes() {
		// 3x3 picture: 2x2 chroma, the last row of each plane without padding.
		let y = [1u8; 4 * 2 + 3];
		let c = [2u8; 3 + 2];
		let ptrs = [y.as_ptr(), c.as_ptr(), c.as_ptr()];
		// SAFETY: each plane is valid for its rows at these strides.
		let planes = unsafe { Planes::from_raw(3, 3, ptrs, [4, 3, 3]) }.unwrap();
		assert_eq!((planes.y.0.len(), planes.u.0.len()), (y.len(), c.len()));
		assert_eq!(yuv_to_rgb8(&planes).len(), 3 * 3 * 3);
	}

	#[test]
	fn raw_planes_reject_invalid_layouts() {
		let y = [0u8; 16];
		let ptrs = [y.as_ptr(); 3];
		for strides in [[-4, 2, 2], [3, 2, 2], [4, 1, 2]] {
			// SAFETY: invalid strides are rejected before any plane is read.
			assert!(unsafe { Planes::from_raw(4, 4, ptrs, strides) }.is_err(), "{strides:?}");
		}
		let ptrs = [y.as_ptr(), std::ptr::null(), y.as_ptr()];
		// SAFETY: a null plane is rejected before any plane is read.
		assert!(unsafe { Planes::from_raw(4, 4, ptrs, [4, 2, 2]) }.is_err());
	}

	#[test]
	fn downscales_with_a_box_filter() {
		// 4x4 luma: a 2x2 checker of 2x2 blocks, which halves into one sample per block.
		let rgb = |v: u8| [v, v, v];
		let pixels: Vec<u8> = (0..4)
			.flat_map(|y| (0..4).flat_map(move |x| rgb(if (x / 2 + y / 2) % 2 == 0 { 0 } else { 255 })))
			.collect();
		let frame = i420(4, 4, PixelFormat::Rgb8, pixels);
		let half = frame.downscale(2, 2);
		assert_eq!((half.width(), half.height()), (2, 2));
		assert_eq!(half.y(), &[16, 235, 235, 16]);
		assert_eq!((half.u(), half.v()), (&[128][..], &[128][..]));

		// Non-integer ratios cover every input row and column.
		let frame = i420(6, 6, PixelFormat::Rgb8, [200u8; 6 * 6 * 3].to_vec());
		let scaled = frame.downscale(4, 2);
		assert!(scaled.y().iter().all(|&y| y == frame.y()[0]), "{:?}", scaled.y());
		assert_eq!(scaled.data().len(), 4 * 2 * 3 / 2);
	}

	#[test]
	fn converts_odd_widths_to_rgb() {
		// 3x2 luma, 2x1 chroma: the last column has a chroma sample of its own.
		let (y, u, v) = ([16, 128, 235, 16, 128, 235], [128, 90], [128, 240]);
		let planes = Planes {
			width: 3,
			height: 2,
			y: (&y, 3),
			u: (&u, 2),
			v: (&v, 2),
		};
		let rgb = yuv_to_rgb8(&planes);
		assert_eq!(rgb.len(), 3 * 2 * 3);
		for (i, pixel) in rgb.as_chunks::<3>().0.iter().enumerate() {
			let (row, col) = (i / 3, i % 3);
			let expected = yuv_to_rgb(y[row * 3 + col], u[col / 2], v[col / 2]);
			assert!(max_diff(pixel, &expected) <= 2, "pixel {i}: {pixel:?} vs {expected:?}");
		}
	}

	/// BT.601 limited-range RGB -> YUV, one pixel at a time.
	#[expect(
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss,
		reason = "the result is in 0..=255"
	)]
	fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
		(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
	}

	#[expect(
		clippy::cast_possible_truncation,
		clippy::cast_sign_loss,
		reason = "the result is in 0..=255"
	)]
	fn rgb_to_uv(r: i32, g: i32, b: i32) -> (u8, u8) {
		let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
		let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
		(u as u8, v as u8)
	}

	/// The SIMD conversions agree with the per-pixel BT.601 formulas: within 1 to YUV, and
	/// within 2 back to RGB.
	#[test]
	#[expect(clippy::cast_possible_truncation, reason = "the pattern wraps around")]
	fn agrees_with_the_reference_formulas() {
		let (w, h) = (64usize, 32usize);
		let rgb: Vec<u8> = (0..w * h * 3).map(|i| (i * 37 % 256) as u8).collect();
		let frame = i420(dim(w), dim(h), PixelFormat::Rgb8, rgb.clone());
		for (i, &y) in frame.y().iter().enumerate() {
			let p = &rgb[i * 3..i * 3 + 3];
			let expected = rgb_to_y(i32::from(p[0]), i32::from(p[1]), i32::from(p[2]));
			assert!(y.abs_diff(expected) <= 1, "y {i}: {y} vs {expected}");
		}
		for (i, (&u, &v)) in frame.u().iter().zip(frame.v()).enumerate() {
			let (cx, cy) = (i % (w / 2), i / (w / 2));
			let sum = |c: usize| -> i32 {
				[(0, 0), (1, 0), (0, 1), (1, 1)]
					.iter()
					.map(|(dx, dy)| i32::from(rgb[((cy * 2 + dy) * w + cx * 2 + dx) * 3 + c]))
					.sum()
			};
			let (eu, ev) = rgb_to_uv((sum(0) + 2) >> 2, (sum(1) + 2) >> 2, (sum(2) + 2) >> 2);
			assert!(
				u.abs_diff(eu) <= 1 && v.abs_diff(ev) <= 1,
				"uv {i}: {u},{v} vs {eu},{ev}"
			);
		}

		let back = yuv_to_rgb8(&frame.planes());
		for (i, pixel) in back.as_chunks::<3>().0.iter().enumerate() {
			let (x, y) = (i % w, i / w);
			let ci = (y / 2) * (w / 2) + x / 2;
			let expected = yuv_to_rgb(frame.y()[i], frame.u()[ci], frame.v()[ci]);
			assert!(max_diff(pixel, &expected) <= 2, "rgb {i}: {pixel:?} vs {expected:?}");
		}
	}

	/// Every packed format comes out of the same picture with its channels in its own order.
	#[test]
	fn converts_to_every_packed_format() {
		// 4x3: odd height, so the last row takes the other path.
		let rgb: Vec<u8> = (0..4 * 3).flat_map(|i: u8| [i * 20, 200 - i * 10, 90]).collect();
		let frame = I420::from_image(&Image::new(4, 3, PixelFormat::Rgb8, rgb, Duration::ZERO).unwrap()).unwrap();
		let planes = frame.planes();
		let reference = yuv_to_rgb8(&planes);
		let pixels = |format| yuv_to_packed(&planes, format).unwrap();
		for (format, order, alpha) in [
			(PixelFormat::Bgr8, [2, 1, 0], None),
			(PixelFormat::Rgba8, [0, 1, 2], Some(3)),
			(PixelFormat::Bgra8, [2, 1, 0], Some(3)),
		] {
			let out = pixels(format);
			let bpp = format.bytes_per_pixel();
			assert_eq!(out.len(), 4 * 2 * bpp, "{format}");
			for (pixel, rgb) in out.chunks_exact(bpp).zip(reference.as_chunks::<3>().0) {
				for channel in 0..3 {
					assert!(
						pixel[order[channel]].abs_diff(rgb[channel]) <= 1,
						"{format}: {pixel:?} vs {rgb:?}"
					);
				}
				if let Some(index) = alpha {
					assert_eq!(pixel[index], 255, "{format}");
				}
			}
		}
		assert!(matches!(
			yuv_to_packed(&planes, PixelFormat::Yuyv),
			Err(Error::InvalidSettings(_))
		));
	}

	#[test]
	fn mono_round_trips_through_luma() {
		let gray: Vec<u8> = (0..=255).collect();
		let frame =
			I420::from_image(&Image::new(16, 16, PixelFormat::Mono8, gray.clone(), Duration::ZERO).unwrap()).unwrap();
		let back = yuv_to_packed(&frame.planes(), PixelFormat::Mono8).unwrap();
		assert!(max_diff(&back, &gray) <= 1, "{back:?}");
		assert_eq!((back[0], back[255]), (0, 255));
	}
}
