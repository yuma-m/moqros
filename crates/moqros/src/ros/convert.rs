//! `sensor_msgs/Image` <-> [`Image`].

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use r2r::builtin_interfaces::msg::Time;
use r2r::std_msgs::msg::Header;

use super::RosImage;
use crate::{Error, Image, Result};

/// Convert a `sensor_msgs/Image` into an [`Image`] without copying the pixel buffer.
pub fn image_from_ros(msg: RosImage) -> Result<Image> {
	if msg.is_bigendian != 0 && msg.encoding.contains("16") {
		return Err(Error::UnsupportedEncoding(format!("{} (big endian)", msg.encoding)));
	}
	let format = msg.encoding.parse()?;
	let stamp = &msg.header.stamp;
	let timestamp = Duration::new(u64::try_from(stamp.sec).unwrap_or(0), stamp.nanosec);
	Image::with_step(msg.width, msg.height, format, msg.step, msg.data, timestamp)
}

/// Convert an [`Image`] into a `sensor_msgs/Image` with the given header.
pub fn image_to_ros(image: &Image, header: Header) -> RosImage {
	RosImage {
		header,
		height: image.height,
		width: image.width,
		encoding: image.format.as_ros_encoding().to_string(),
		is_bigendian: 0,
		step: image.step,
		data: image.data.to_vec(),
	}
}

/// A `builtin_interfaces/Time` for the current wall clock, saturating past 2038.
pub fn now_stamp() -> Time {
	let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
	Time {
		sec: i32::try_from(now.as_secs()).unwrap_or(i32::MAX),
		nanosec: now.subsec_nanos(),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::PixelFormat;

	#[expect(clippy::cast_possible_truncation, reason = "the test data wraps around")]
	fn ros_image(encoding: &str, width: u32, height: u32, step: u32) -> RosImage {
		RosImage {
			header: Header {
				stamp: Time { sec: 12, nanosec: 34 },
				frame_id: "camera".into(),
			},
			height,
			width,
			encoding: encoding.into(),
			is_bigendian: 0,
			step,
			data: (0..step * height).map(|i| i as u8).collect(),
		}
	}
	#[test]
	fn converts_ros_images() {
		let image = image_from_ros(ros_image("bgr8", 2, 2, 8)).unwrap();
		assert_eq!(
			(image.width, image.height, image.format, image.step),
			(2, 2, PixelFormat::Bgr8, 8)
		);
		assert_eq!(image.timestamp, Duration::new(12, 34));
		assert_eq!(image.data.len(), 16);

		let header = Header {
			stamp: Time { sec: 1, nanosec: 2 },
			frame_id: "out".into(),
		};
		let back = image_to_ros(&image, header.clone());
		assert_eq!(back.header, header);
		assert_eq!(back.encoding, "bgr8");
		assert_eq!((back.width, back.height, back.step, back.is_bigendian), (2, 2, 8, 0));
		assert_eq!(back.data, ros_image("bgr8", 2, 2, 8).data);
	}

	#[test]
	fn clamps_negative_stamps() {
		let mut msg = ros_image("mono8", 2, 2, 2);
		msg.header.stamp.sec = -5;
		assert_eq!(image_from_ros(msg).unwrap().timestamp, Duration::new(0, 34));
	}

	#[test]
	fn rejects_unsupported_ros_images() {
		assert!(matches!(
			image_from_ros(ros_image("16UC1", 2, 2, 4)),
			Err(Error::UnsupportedEncoding(_))
		));
		let mut big_endian = ros_image("mono16", 2, 2, 4);
		big_endian.is_bigendian = 1;
		assert!(matches!(image_from_ros(big_endian), Err(Error::UnsupportedEncoding(e)) if e.contains("big endian")));
		assert!(matches!(
			image_from_ros(ros_image("rgb8", 4, 2, 8)),
			Err(Error::InvalidImage(_))
		));
	}
}
