//! Publish images to a MoQ relay as hang (WebCodecs-compatible) VP8, H.264, or AV1 broadcasts,
//! and other messages as hang data broadcasts.

use hang::catalog::{PRIORITY, VideoCodec};
use hang::moq_net;

use crate::catalog::{self, VIDEO_TRACK};
use crate::clock::StreamClock;
use crate::codec::{EncodedFrame, Encoder, EncoderSettings};
use crate::data::{DataPublisher, DataSettings};
use crate::track::{BroadcastWriter, GroupWriter};
use crate::{ClientConfig, Error, Image, Result, Session};

/// A connection to a relay that can carry any number of image and data broadcasts.
///
/// The connection redials in the background if it drops; broadcasts created
/// here are re-announced automatically once it is back. Clones share the
/// connection, which stays up while any clone is alive.
#[derive(Clone)]
pub struct Publisher {
	origin: moq_net::origin::Producer,
	session: Session,
}

impl Publisher {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config.client()?.with_publisher(&origin).connect(config.url.clone());
		Ok(Self {
			origin,
			session: Session::new(connection),
		})
	}

	pub(crate) fn from_parts(origin: moq_net::origin::Producer, session: Session) -> Self {
		Self { origin, session }
	}

	/// The relay connection, shared with every clone.
	pub fn session(&self) -> &Session {
		&self.session
	}

	/// See [`Session::established`].
	pub async fn established(&self) -> Result<()> {
		self.session.established().await
	}

	/// See [`Session::closed`].
	pub async fn closed(&self) -> Result<()> {
		self.session.closed().await
	}

	/// See [`Session::close`].
	pub async fn close(&self) -> Result<()> {
		self.session.close().await
	}

	/// Create and announce a broadcast named `name` that carries encoded images.
	///
	/// Fails if `settings.codec` isn't compiled into this build.
	pub fn create_image_broadcast(&self, name: &str, settings: EncoderSettings) -> Result<ImagePublisher> {
		ImagePublisher::new(&self.origin, name, settings)
	}

	/// Create and announce a broadcast named `name` that carries messages in a data track.
	pub fn create_data_broadcast(&self, name: &str, settings: DataSettings) -> Result<DataPublisher> {
		DataPublisher::new(&self.origin, name, settings)
	}
}

/// Encodes images and writes them into one MoQ broadcast.
///
/// The codec is chosen by [`EncoderSettings::codec`] (VP8 by default).
///
/// Every keyframe starts a new MoQ group so subscribers (and relays) can join
/// or skip ahead at group boundaries. The catalog is republished whenever the
/// resolution or codec parameters change.
pub struct ImagePublisher {
	// Declared before `broadcast`, so the open group is finished before the broadcast closes.
	video: GroupWriter,
	broadcast: BroadcastWriter,
	encoder: Encoder,
	rendition: Option<(u32, u32, VideoCodec)>,
	clock: StreamClock,
}

impl ImagePublisher {
	fn new(origin: &moq_net::origin::Producer, name: &str, settings: EncoderSettings) -> Result<Self> {
		let encoder = Encoder::new(settings)?;
		let broadcast = BroadcastWriter::create(origin, name)?;
		let video = broadcast.create_track(VIDEO_TRACK, hang::container::track_info(PRIORITY.video))?;
		tracing::info!(broadcast = name, codec = %encoder.settings().codec, "announced image broadcast");

		Ok(Self {
			video: GroupWriter::new(video),
			broadcast,
			encoder,
			rendition: None,
			clock: StreamClock::default(),
		})
	}

	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		self.broadcast.name()
	}

	/// Encode and send one image. Returns whether a frame was written.
	///
	/// Encoding is CPU bound; call this from a blocking-friendly thread when
	/// running inside an async runtime.
	pub fn publish(&mut self, image: &Image) -> Result<bool> {
		match self.encoder.encode(image)? {
			Some(frame) => {
				self.publish_encoded(&frame)?;
				Ok(true)
			}
			None => Ok(false),
		}
	}

	/// Send an already encoded frame (VP8, Annex-B H.264 with SPS/PPS inline in keyframes, or an
	/// AV1 temporal unit with the sequence header in keyframes).
	pub fn publish_encoded(&mut self, frame: &EncodedFrame) -> Result<()> {
		if frame.keyframe {
			self.update_catalog(frame)?;
			self.video.start_group()?;
		} else if self.video.is_open() && !self.video.fits(frame.data.len()) {
			// moq-net would abort the group on this write. Close it and drop frames
			// until the next keyframe opens a new one.
			tracing::warn!(
				broadcast = %self.name(),
				"video group is full; raise the bitrate budget or shorten the keyframe interval"
			);
			self.video.finish_group()?;
		}

		// Nothing decodable can be sent until the first keyframe opens a group.
		if !self.video.is_open() {
			self.encoder.force_keyframe();
			return Ok(());
		}

		let timestamp = self.clock.micros(frame.timestamp);
		self.video.write_container(&hang::container::Frame {
			timestamp: moq_net::Timestamp::from_micros(timestamp).map_err(|_| Error::Timestamp)?,
			payload: frame.data.clone(),
		})?;
		// Start the next group early, so a long keyframe interval doesn't fill this one.
		if self.video.nearly_full() {
			self.encoder.force_keyframe();
		}
		Ok(())
	}

	fn update_catalog(&mut self, frame: &EncodedFrame) -> Result<()> {
		let codec = frame.codec.clone();
		let rendition = (frame.width, frame.height, codec.clone());
		if self.rendition.as_ref() == Some(&rendition) {
			return Ok(());
		}

		let catalog = catalog::video(frame, self.encoder.settings())?;
		self.broadcast.publish_catalog(&catalog)?;

		tracing::info!(
			broadcast = %self.name(),
			codec = %codec,
			width = frame.width,
			height = frame.height,
			"published catalog"
		);
		self.rendition = Some(rendition);
		Ok(())
	}
}

#[cfg(test)]
mod tests {
	use std::time::Duration;

	use bytes::Bytes;

	use super::*;
	use crate::codec::Codec;

	fn publisher(settings: EncoderSettings) -> ImagePublisher {
		ImagePublisher::new(&moq_tokio::origin::spawn(), "camera", settings).unwrap()
	}

	/// A keyframe interval too long for one group starts the next group early.
	#[tokio::test]
	async fn long_keyframe_interval_starts_groups_early() {
		if Codec::available().is_empty() {
			return;
		}
		let mut publisher = publisher(EncoderSettings {
			keyframe_interval: Duration::from_secs(3600),
			..Default::default()
		});
		let mut keyframes = 0;
		for t in 0..moq_net::group::MAX_GROUP_FRAMES as u32 + 100 {
			publisher
				.publish(&crate::codec::tests::test_pattern(16, 16, t))
				.unwrap();
			keyframes += usize::from(publisher.video.frames() == 1);
		}
		assert_eq!(keyframes, 2);
	}

	/// Encoded frames that would overflow a group are dropped until the next keyframe.
	#[tokio::test]
	async fn full_group_waits_for_a_keyframe() {
		if Codec::available().is_empty() {
			return;
		}
		let mut publisher = publisher(EncoderSettings::default());
		let frame = |n: u64, keyframe: bool| EncodedFrame {
			data: Bytes::from_static(&[0; 16]),
			keyframe,
			width: 16,
			height: 16,
			timestamp: Duration::from_millis(n * 33),
			codec: VideoCodec::VP8,
		};
		let limit = moq_net::group::MAX_GROUP_FRAMES as u64;
		for n in 0..limit + 10 {
			publisher.publish_encoded(&frame(n, n == 0)).unwrap();
		}
		assert!(!publisher.video.is_open());
		publisher.publish_encoded(&frame(limit + 10, true)).unwrap();
		assert_eq!(publisher.video.frames(), 1);
	}
}
