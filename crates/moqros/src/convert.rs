//! Color conversion between [`Image`] pixel formats and planar I420 (YUV 4:2:0).
//!
//! Uses BT.601 limited-range integer math, which is what VP8/H.264 decoders (and browsers)
//! assume when the bitstream carries no color description.

use crate::{Error, Image, PixelFormat};

/// A tightly packed I420 frame with even dimensions: the Y plane, then U, then V,
/// in one contiguous buffer (the layout `vpx_img_wrap` expects).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I420 {
	width: usize,
	height: usize,
	data: Vec<u8>,
}

impl I420 {
	pub fn width(&self) -> usize {
		self.width
	}

	pub fn height(&self) -> usize {
		self.height
	}

	/// The whole contiguous Y+U+V buffer.
	pub fn data(&self) -> &[u8] {
		&self.data
	}

	pub fn y(&self) -> &[u8] {
		&self.data[..self.luma_len()]
	}

	pub fn u(&self) -> &[u8] {
		let (l, c) = (self.luma_len(), self.luma_len() / 4);
		&self.data[l..l + c]
	}

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

	/// Convert any supported [`Image`] to I420.
	///
	/// 4:2:0 video needs even dimensions, so an odd trailing row or column is cropped.
	pub fn from_image(image: &Image) -> Result<Self, Error> {
		image.validate()?;
		let width = image.width as usize & !1;
		let height = image.height as usize & !1;
		if width == 0 || height == 0 {
			return Err(Error::InvalidImage(format!(
				"{}x{} is too small to encode",
				image.width, image.height
			)));
		}

		let mut out = Self {
			width,
			height,
			data: vec![0; width * height * 3 / 2],
		};

		match image.format {
			PixelFormat::Rgb8 => out.fill_rgb(image, 3, [0, 1, 2]),
			PixelFormat::Bgr8 => out.fill_rgb(image, 3, [2, 1, 0]),
			PixelFormat::Rgba8 => out.fill_rgb(image, 4, [0, 1, 2]),
			PixelFormat::Bgra8 => out.fill_rgb(image, 4, [2, 1, 0]),
			PixelFormat::Mono8 => out.fill_mono(image),
			// Offsets of (Y0, U, Y1, V) inside each 4-byte macropixel.
			PixelFormat::Yuyv => out.fill_packed_422(image, [0, 1, 2, 3]),
			PixelFormat::Uyvy => out.fill_packed_422(image, [1, 0, 3, 2]),
		}

		Ok(out)
	}

	fn fill_rgb(&mut self, image: &Image, bpp: usize, [r, g, b]: [usize; 3]) {
		let (width, height) = (self.width, self.height);
		let cw = width / 2;
		let (y_plane, u_plane, v_plane) = self.planes_mut();
		for cy in 0..height / 2 {
			let rows = [image.row(cy * 2), image.row(cy * 2 + 1)];
			for cx in 0..cw {
				let (mut sr, mut sg, mut sb) = (0i32, 0i32, 0i32);
				for (dy, row) in rows.iter().enumerate() {
					for dx in 0..2 {
						let x = cx * 2 + dx;
						let px = &row[x * bpp..];
						let (pr, pg, pb) = (px[r] as i32, px[g] as i32, px[b] as i32);
						y_plane[(cy * 2 + dy) * width + x] = rgb_to_y(pr, pg, pb);
						sr += pr;
						sg += pg;
						sb += pb;
					}
				}
				let (u, v) = rgb_to_uv((sr + 2) >> 2, (sg + 2) >> 2, (sb + 2) >> 2);
				u_plane[cy * cw + cx] = u;
				v_plane[cy * cw + cx] = v;
			}
		}
	}

	fn fill_mono(&mut self, image: &Image) {
		let (width, height) = (self.width, self.height);
		let (y_plane, u_plane, v_plane) = self.planes_mut();
		for y in 0..height {
			let row = image.row(y);
			let dst = &mut y_plane[y * width..(y + 1) * width];
			for (d, &s) in dst.iter_mut().zip(row) {
				*d = (16 + ((s as u32 * 219 + 127) / 255)) as u8;
			}
		}
		u_plane.fill(128);
		v_plane.fill(128);
	}

	fn fill_packed_422(&mut self, image: &Image, [y0, u, y1, v]: [usize; 4]) {
		let (width, height) = (self.width, self.height);
		let cw = width / 2;
		let (y_plane, u_plane, v_plane) = self.planes_mut();
		for cy in 0..height / 2 {
			let rows = [image.row(cy * 2), image.row(cy * 2 + 1)];
			for cx in 0..cw {
				let mut su = 0u32;
				let mut sv = 0u32;
				for (dy, row) in rows.iter().enumerate() {
					let mp = &row[cx * 4..cx * 4 + 4];
					let base = (cy * 2 + dy) * width + cx * 2;
					y_plane[base] = mp[y0];
					y_plane[base + 1] = mp[y1];
					su += mp[u] as u32;
					sv += mp[v] as u32;
				}
				u_plane[cy * cw + cx] = su.div_ceil(2) as u8;
				v_plane[cy * cw + cx] = sv.div_ceil(2) as u8;
			}
		}
	}
}

/// Borrowed 4:2:0 planes with arbitrary strides, e.g. a decoder's output picture.
#[derive(Clone, Copy, Debug)]
pub struct Planes<'a> {
	pub width: usize,
	pub height: usize,
	/// (plane, stride in bytes)
	pub y: (&'a [u8], usize),
	pub u: (&'a [u8], usize),
	pub v: (&'a [u8], usize),
}

/// Convert planar 4:2:0 into packed `rgb8`.
pub fn yuv_to_rgb8(src: &Planes<'_>) -> Vec<u8> {
	let (width, height) = (src.width, src.height);
	let ((y_plane, ys), (u_plane, us), (v_plane, vs)) = (src.y, src.u, src.v);

	let mut out = vec![0u8; width * height * 3];
	for row in 0..height {
		let y_row = &y_plane[row * ys..];
		let u_row = &u_plane[(row / 2) * us..];
		let v_row = &v_plane[(row / 2) * vs..];
		let dst = &mut out[row * width * 3..(row + 1) * width * 3];
		for col in 0..width {
			let [r, g, b] = yuv_to_rgb(y_row[col], u_row[col / 2], v_row[col / 2]);
			dst[col * 3] = r;
			dst[col * 3 + 1] = g;
			dst[col * 3 + 2] = b;
		}
	}
	out
}

#[inline]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
	(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8
}

#[inline]
fn rgb_to_uv(r: i32, g: i32, b: i32) -> (u8, u8) {
	let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
	let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
	(u as u8, v as u8)
}

#[inline]
fn yuv_to_rgb(y: u8, u: u8, v: u8) -> [u8; 3] {
	let c = y as i32 - 16;
	let d = u as i32 - 128;
	let e = v as i32 - 128;
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
}
