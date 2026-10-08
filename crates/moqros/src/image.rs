//! ROS-independent raw image representation.

use std::fmt;
use std::str::FromStr;
use std::time::Duration;

use bytes::Bytes;

use crate::Error;

/// Pixel layouts understood by moqros.
///
/// The names mirror the `encoding` strings of `sensor_msgs/Image`
/// (see `sensor_msgs/image_encodings.hpp`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PixelFormat {
	/// `rgb8`: 3 bytes per pixel, R G B.
	Rgb8,
	/// `bgr8`: 3 bytes per pixel, B G R.
	Bgr8,
	/// `rgba8`: 4 bytes per pixel, R G B A.
	Rgba8,
	/// `bgra8`: 4 bytes per pixel, B G R A.
	Bgra8,
	/// `mono8`: 1 byte per pixel, luma only.
	Mono8,
	/// `yuv422`: packed 4:2:2, byte order U Y0 V Y1 (a.k.a. UYVY).
	Uyvy,
	/// `yuv422_yuy2`: packed 4:2:2, byte order Y0 U Y1 V (a.k.a. YUYV).
	Yuyv,
}

impl PixelFormat {
	/// The `sensor_msgs/Image` encoding string.
	pub const fn as_ros_encoding(self) -> &'static str {
		match self {
			Self::Rgb8 => "rgb8",
			Self::Bgr8 => "bgr8",
			Self::Rgba8 => "rgba8",
			Self::Bgra8 => "bgra8",
			Self::Mono8 => "mono8",
			Self::Uyvy => "yuv422",
			Self::Yuyv => "yuv422_yuy2",
		}
	}

	/// Average number of bytes used by one pixel in a row.
	pub const fn bytes_per_pixel(self) -> usize {
		match self {
			Self::Rgb8 | Self::Bgr8 => 3,
			Self::Rgba8 | Self::Bgra8 => 4,
			Self::Mono8 => 1,
			Self::Uyvy | Self::Yuyv => 2,
		}
	}
}

impl FromStr for PixelFormat {
	type Err = Error;

	fn from_str(s: &str) -> Result<Self, Self::Err> {
		Ok(match s {
			"rgb8" | "8UC3" => Self::Rgb8,
			"bgr8" => Self::Bgr8,
			"rgba8" | "8UC4" => Self::Rgba8,
			"bgra8" => Self::Bgra8,
			"mono8" | "8UC1" => Self::Mono8,
			"yuv422" | "uyvy" => Self::Uyvy,
			"yuv422_yuy2" | "yuyv" => Self::Yuyv,
			other => return Err(Error::UnsupportedEncoding(other.to_string())),
		})
	}
}

impl fmt::Display for PixelFormat {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.write_str(self.as_ros_encoding())
	}
}

/// An uncompressed image, equivalent to the payload of a `sensor_msgs/Image`.
#[derive(Clone, PartialEq, Eq)]
pub struct Image {
	pub width: u32,
	pub height: u32,
	pub format: PixelFormat,
	/// Length of one row in bytes, including any padding.
	pub step: u32,
	pub data: Bytes,
	/// Capture time; for ROS images this is `header.stamp` since the epoch.
	pub timestamp: Duration,
}

impl Image {
	/// Create a tightly packed image, validating the buffer length.
	pub fn new(
		width: u32,
		height: u32,
		format: PixelFormat,
		data: impl Into<Bytes>,
		timestamp: Duration,
	) -> Result<Self, Error> {
		let step = width * format.bytes_per_pixel() as u32;
		Self::with_step(width, height, format, step, data, timestamp)
	}

	/// Create an image whose rows are `step` bytes apart, validating the buffer length.
	pub fn with_step(
		width: u32,
		height: u32,
		format: PixelFormat,
		step: u32,
		data: impl Into<Bytes>,
		timestamp: Duration,
	) -> Result<Self, Error> {
		let image = Self {
			width,
			height,
			format,
			step,
			data: data.into(),
			timestamp,
		};
		image.validate()?;
		Ok(image)
	}

	/// Check that `step` and `data` are large enough for the declared geometry.
	pub fn validate(&self) -> Result<(), Error> {
		let row = self.width as usize * self.format.bytes_per_pixel();
		let step = self.step as usize;
		let needed = step * self.height as usize;
		if step < row || self.data.len() < needed {
			return Err(Error::InvalidImage(format!(
				"{}x{} {} needs step >= {row} and {needed} bytes, got step {step} and {} bytes",
				self.width,
				self.height,
				self.format,
				self.data.len()
			)));
		}
		Ok(())
	}

	/// Borrow row `y` without trailing padding.
	pub(crate) fn row(&self, y: usize) -> &[u8] {
		let start = y * self.step as usize;
		&self.data[start..start + self.width as usize * self.format.bytes_per_pixel()]
	}
}

impl fmt::Debug for Image {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		f.debug_struct("Image")
			.field("width", &self.width)
			.field("height", &self.height)
			.field("format", &self.format)
			.field("step", &self.step)
			.field("data", &format_args!("{} bytes", self.data.len()))
			.field("timestamp", &self.timestamp)
			.finish()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_ros_encodings() {
		for format in [
			PixelFormat::Rgb8,
			PixelFormat::Bgr8,
			PixelFormat::Rgba8,
			PixelFormat::Bgra8,
			PixelFormat::Mono8,
			PixelFormat::Uyvy,
			PixelFormat::Yuyv,
		] {
			assert_eq!(format.as_ros_encoding().parse::<PixelFormat>().unwrap(), format);
		}
		assert!("32FC1".parse::<PixelFormat>().is_err());
	}

	#[test]
	fn rejects_short_buffers() {
		assert!(Image::new(4, 4, PixelFormat::Rgb8, vec![0u8; 47], Duration::ZERO).is_err());
		assert!(Image::new(4, 4, PixelFormat::Rgb8, vec![0u8; 48], Duration::ZERO).is_ok());
		assert!(Image::with_step(4, 4, PixelFormat::Rgb8, 11, vec![0u8; 64], Duration::ZERO).is_err());
	}
}
