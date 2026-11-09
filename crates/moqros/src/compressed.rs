//! JPEG and PNG images, as `sensor_msgs/CompressedImage` carries them (feature `compressed`).

use std::io::Cursor;
use std::time::Duration;

use zune_jpeg::JpegDecoder;
use zune_jpeg::zune_core::bytestream::ZCursor;
use zune_jpeg::zune_core::colorspace::ColorSpace;
use zune_jpeg::zune_core::options::DecoderOptions;

use crate::codec::check_size;
use crate::{Error, Image, PixelFormat, Result};

/// Compression of an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Compression {
	Jpeg,
	Png,
}

impl Image {
	/// Decode a JPEG or PNG image, e.g. the `data` of a `sensor_msgs/CompressedImage` whose
	/// `format` is `format` (`jpeg`, `png`, or image_transport's `bgr8; jpeg compressed bgr8`).
	///
	/// The compression is told by the data itself; `format` only rules out what moqros can't
	/// decode (`compressedDepth`). Color images decode to `rgb8` (`rgba8` for PNGs with alpha) and
	/// grayscale ones to `mono8`. An image of over `max_pixels` fails with [`Error::TooLarge`]
	/// before it is decoded.
	pub fn from_compressed(format: &str, data: &[u8], timestamp: Duration, max_pixels: u64) -> Result<Self> {
		if format.to_ascii_lowercase().contains("compresseddepth") {
			return Err(Error::UnsupportedEncoding(format!("{format} (compressed depth)")));
		}
		match sniff(data) {
			Some(Compression::Jpeg) => decode_jpeg(data, timestamp, max_pixels),
			Some(Compression::Png) => decode_png(data, timestamp, max_pixels),
			None => Err(Error::UnsupportedEncoding(format!(
				"{format} (neither JPEG nor PNG data)"
			))),
		}
	}
}

/// The compression of `data`, by its signature.
fn sniff(data: &[u8]) -> Option<Compression> {
	if data.starts_with(&[0xff, 0xd8, 0xff]) {
		Some(Compression::Jpeg)
	} else if data.starts_with(b"\x89PNG\r\n\x1a\n") {
		Some(Compression::Png)
	} else {
		None
	}
}

/// Image dimensions as `u32`, failing the size check for anything larger.
fn dims(width: usize, height: usize) -> (u32, u32) {
	let dim = |n: usize| u32::try_from(n).unwrap_or(u32::MAX);
	(dim(width), dim(height))
}

fn decode_jpeg(data: &[u8], timestamp: Duration, max_pixels: u64) -> Result<Image> {
	let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), DecoderOptions::default());
	decoder.decode_headers().map_err(jpeg_error)?;
	let info = decoder
		.info()
		.ok_or_else(|| Error::InvalidImage("JPEG without a frame header".into()))?;
	let (width, height) = dims(usize::from(info.width), usize::from(info.height));
	check_size(width, height, max_pixels)?;

	let (format, colorspace) = match decoder.input_colorspace() {
		Some(ColorSpace::Luma) => (PixelFormat::Mono8, ColorSpace::Luma),
		_ => (PixelFormat::Rgb8, ColorSpace::RGB),
	};
	let options = DecoderOptions::default()
		.jpeg_set_out_colorspace(colorspace)
		.set_max_width(width as usize)
		.set_max_height(height as usize);
	let mut decoder = JpegDecoder::new_with_options(ZCursor::new(data), options);
	let pixels = decoder.decode().map_err(jpeg_error)?;
	Image::new(width, height, format, pixels, timestamp)
}

fn jpeg_error(err: zune_jpeg::errors::DecodeErrors) -> Error {
	Error::InvalidImage(format!("JPEG: {err}"))
}

fn decode_png(data: &[u8], timestamp: Duration, max_pixels: u64) -> Result<Image> {
	// Allocations stay within what a `max_pixels` rgba8 picture needs.
	let limits = png::Limits {
		bytes: usize::try_from(max_pixels.saturating_mul(4)).unwrap_or(usize::MAX),
	};
	let mut decoder = png::Decoder::new_with_limits(Cursor::new(data), limits);
	decoder.set_transformations(png::Transformations::normalize_to_color8());
	let mut reader = decoder.read_info().map_err(png_error)?;
	let info = reader.info();
	let (width, height) = dims(info.width as usize, info.height as usize);
	check_size(width, height, max_pixels)?;

	let size = reader
		.output_buffer_size()
		.ok_or_else(|| Error::InvalidImage("PNG too large".into()))?;
	let mut pixels = vec![0; size];
	let output = reader.next_frame(&mut pixels).map_err(png_error)?;
	pixels.truncate(output.buffer_size());
	let mut step = output.line_size;
	let format = match output.color_type {
		png::ColorType::Grayscale => PixelFormat::Mono8,
		// mono8 has no alpha: keep the luma of each pixel.
		png::ColorType::GrayscaleAlpha => {
			pixels = pixels.as_chunks::<2>().0.iter().map(|[luma, _]| *luma).collect();
			step /= 2;
			PixelFormat::Mono8
		}
		png::ColorType::Rgb => PixelFormat::Rgb8,
		png::ColorType::Rgba => PixelFormat::Rgba8,
		png::ColorType::Indexed => return Err(Error::InvalidImage("PNG palette wasn't expanded".into())),
	};
	let step = u32::try_from(step).map_err(|_| Error::InvalidImage("PNG rows too long".into()))?;
	Image::with_step(width, height, format, step, pixels, timestamp)
}

fn png_error(err: png::DecodingError) -> Error {
	Error::InvalidImage(format!("PNG: {err}"))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::codec::DEFAULT_MAX_PIXELS;

	/// A 16x8 baseline JPEG: red on the left half, blue on the right.
	const JPEG_RGB: &[u8] = include_bytes!("../tests/fixtures/red_blue_16x8.jpg");
	/// The same picture in grayscale.
	const JPEG_GRAY: &[u8] = include_bytes!("../tests/fixtures/gray_16x8.jpg");

	fn png(width: u32, height: u32, color: png::ColorType, pixels: &[u8]) -> Vec<u8> {
		let mut out = Vec::new();
		let mut encoder = png::Encoder::new(&mut out, width, height);
		encoder.set_color(color);
		encoder.set_depth(png::BitDepth::Eight);
		encoder.write_header().unwrap().write_image_data(pixels).unwrap();
		out
	}

	#[test]
	fn decodes_jpeg() {
		let image = Image::from_compressed(
			"bgr8; jpeg compressed bgr8",
			JPEG_RGB,
			Duration::from_secs(3),
			DEFAULT_MAX_PIXELS,
		)
		.unwrap();
		assert_eq!((image.width, image.height, image.format), (16, 8, PixelFormat::Rgb8));
		assert_eq!(image.timestamp, Duration::from_secs(3));
		let pixel = |x: usize| &image.data[x * 3..x * 3 + 3];
		assert!(pixel(1)[0] > 200 && pixel(1)[2] < 60, "left is red: {:?}", pixel(1));
		assert!(
			pixel(14)[2] > 200 && pixel(14)[0] < 60,
			"right is blue: {:?}",
			pixel(14)
		);

		let gray = Image::from_compressed("jpeg", JPEG_GRAY, Duration::ZERO, DEFAULT_MAX_PIXELS).unwrap();
		assert_eq!((gray.width, gray.height, gray.format), (16, 8, PixelFormat::Mono8));
	}

	#[test]
	fn decodes_png() {
		let rgb: Vec<u8> = (0..4 * 2 * 3).map(|i: u8| i * 10).collect();
		let image = Image::from_compressed(
			"png",
			&png(4, 2, png::ColorType::Rgb, &rgb),
			Duration::ZERO,
			DEFAULT_MAX_PIXELS,
		)
		.unwrap();
		assert_eq!((image.width, image.height, image.format), (4, 2, PixelFormat::Rgb8));
		assert_eq!(&image.data[..], &rgb[..]);

		let gray_alpha = [10, 255, 20, 0, 30, 128, 40, 1];
		let image = Image::from_compressed(
			"mono8; png compressed mono8",
			&png(2, 2, png::ColorType::GrayscaleAlpha, &gray_alpha),
			Duration::ZERO,
			DEFAULT_MAX_PIXELS,
		)
		.unwrap();
		assert_eq!(
			(image.format, &image.data[..]),
			(PixelFormat::Mono8, &[10, 20, 30, 40][..])
		);

		let rgba = [1, 2, 3, 4];
		let image = Image::from_compressed(
			"png",
			&png(1, 1, png::ColorType::Rgba, &rgba),
			Duration::ZERO,
			DEFAULT_MAX_PIXELS,
		)
		.unwrap();
		assert_eq!((image.format, &image.data[..]), (PixelFormat::Rgba8, &rgba[..]));
	}

	#[test]
	fn rejects_oversized_images_before_decoding() {
		assert!(matches!(
			Image::from_compressed("jpeg", JPEG_RGB, Duration::ZERO, 16 * 8 - 1),
			Err(Error::TooLarge {
				width: 16,
				height: 8,
				..
			})
		));
		let png = png(4, 2, png::ColorType::Rgb, &[0; 24]);
		assert!(matches!(
			Image::from_compressed("png", &png, Duration::ZERO, 7),
			Err(Error::TooLarge {
				width: 4,
				height: 2,
				..
			})
		));
	}

	#[test]
	fn rejects_what_it_cannot_decode() {
		let png = png(1, 1, png::ColorType::Grayscale, &[0]);
		for (format, data) in [
			("16UC1; compressedDepth png", &png[..]),
			("jpeg", b"not an image".as_slice()),
			("tiff", b"II*\0".as_slice()),
		] {
			assert!(
				matches!(
					Image::from_compressed(format, data, Duration::ZERO, DEFAULT_MAX_PIXELS),
					Err(Error::UnsupportedEncoding(_))
				),
				"{format}"
			);
		}
		// Truncated data doesn't panic: JPEG decoding is lenient, PNG decoding fails.
		let _ = Image::from_compressed(
			"jpeg",
			&JPEG_RGB[..JPEG_RGB.len() / 2],
			Duration::ZERO,
			DEFAULT_MAX_PIXELS,
		);
		assert!(Image::from_compressed("png", &png[..png.len() - 8], Duration::ZERO, DEFAULT_MAX_PIXELS).is_err());
	}
}
