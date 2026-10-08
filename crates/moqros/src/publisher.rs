//! Publish images to a MoQ relay as hang (WebCodecs-compatible) H.264 broadcasts.

use std::time::Duration;

use hang::catalog::{Catalog, Container, PRIORITY, VideoCodec, VideoConfig};
use hang::moq_net;

use crate::codec::{EncodedFrame, Encoder, EncoderSettings};
use crate::{ClientConfig, Error, Image, Result};

/// Name of the single video track inside every moqros broadcast.
pub const VIDEO_TRACK: &str = "video";

/// A connection to a relay that can carry any number of image broadcasts.
///
/// The connection redials in the background if it drops; broadcasts created
/// here are re-announced automatically once it is back.
pub struct Publisher {
	origin: moq_net::origin::Producer,
	connection: moq_tokio::Connection,
}

impl Publisher {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config.client()?.with_publisher(&origin).connect(config.url.clone());
		Ok(Self { origin, connection })
	}

	/// Wait until the first session with the relay is established.
	pub async fn established(&self) -> Result<()> {
		// The returned handle is a clone; `self` keeps the dial alive.
		let _ = self.connection.clone().established().await?;
		Ok(())
	}

	/// Resolves when the connection gives up for good.
	pub async fn closed(&self) -> Result<()> {
		Ok(self.connection.closed().await?)
	}

	/// Create and announce a broadcast named `name` that carries encoded images.
	///
	/// Fails if `settings.codec` isn't compiled into this build.
	pub fn create_image_broadcast(&self, name: &str, settings: EncoderSettings) -> Result<ImagePublisher> {
		let encoder = Encoder::new(settings)?;
		let broadcast = self.origin.create_broadcast(name)?;
		broadcast.announce(Default::default())?;

		let catalog = broadcast.create_track(Catalog::DEFAULT_NAME, Catalog::default_track_info())?;
		let video = broadcast.create_track(VIDEO_TRACK, hang::container::track_info(PRIORITY.video))?;
		tracing::info!(broadcast = name, codec = %encoder.settings().codec, "announced image broadcast");

		Ok(ImagePublisher {
			name: name.to_string(),
			broadcast,
			catalog,
			video,
			group: None,
			encoder,
			rendition: None,
			clock: StreamClock::default(),
		})
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
	name: String,
	broadcast: moq_net::broadcast::Producer,
	catalog: moq_net::track::Producer,
	video: moq_net::track::Producer,
	group: Option<moq_net::group::Producer>,
	encoder: Encoder,
	rendition: Option<(u32, u32, VideoCodec)>,
	clock: StreamClock,
}

impl ImagePublisher {
	/// Broadcast name, relative to the relay URL.
	pub fn name(&self) -> &str {
		&self.name
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

	/// Send an already encoded frame (VP8, or Annex-B H.264 with SPS/PPS inline in keyframes).
	pub fn publish_encoded(&mut self, frame: &EncodedFrame) -> Result<()> {
		if frame.keyframe {
			self.update_catalog(frame)?;
			if let Some(group) = self.group.take() {
				group.finish()?;
			}
			self.group = Some(self.video.append_group()?);
		}

		// Nothing decodable can be sent until the first keyframe opens a group.
		let Some(group) = self.group.as_mut() else {
			self.encoder.force_keyframe();
			return Ok(());
		};

		let timestamp = self.clock.micros(frame.timestamp);
		let frame = hang::container::Frame {
			timestamp: moq_net::Timestamp::from_micros(timestamp).map_err(|_| Error::Timestamp)?,
			payload: frame.data.clone(),
		};
		frame.write_to(group)?;
		Ok(())
	}

	fn update_catalog(&mut self, frame: &EncodedFrame) -> Result<()> {
		let codec = frame.codec.clone();
		let rendition = (frame.width, frame.height, codec.clone());
		if self.rendition.as_ref() == Some(&rendition) {
			return Ok(());
		}

		let settings = self.encoder.settings();
		let mut config = VideoConfig::new(codec.clone());
		config.coded_width = Some(frame.width);
		config.coded_height = Some(frame.height);
		config.bitrate = Some(settings.bitrate as u64);
		config.framerate = Some(settings.max_fps as f64);
		config.container = Container::Legacy;

		let mut catalog = Catalog::<()>::default();
		catalog.video.insert(VIDEO_TRACK, config)?;

		let mut group = self.catalog.append_group()?;
		group.write_frame(moq_net::Timestamp::now(), catalog.to_json()?)?;
		group.finish()?;

		tracing::info!(
			broadcast = %self.name,
			codec = %codec,
			width = frame.width,
			height = frame.height,
			"published catalog"
		);
		self.rendition = Some(rendition);
		Ok(())
	}
}

impl Drop for ImagePublisher {
	fn drop(&mut self) {
		if let Some(group) = self.group.take() {
			let _ = group.finish();
		}
		self.broadcast.close();
	}
}

/// Maps capture timestamps onto a strictly increasing microsecond timeline
/// starting at zero, tolerating clock jumps and duplicate stamps.
#[derive(Default)]
struct StreamClock {
	origin: Option<Duration>,
	last: Option<u64>,
}

impl StreamClock {
	fn micros(&mut self, capture: Duration) -> u64 {
		let origin = *self.origin.get_or_insert(capture);
		let mut micros = capture.saturating_sub(origin).as_micros() as u64;
		if let Some(last) = self.last
			&& micros <= last
		{
			micros = last + 1;
		}
		self.last = Some(micros);
		micros
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stream_clock_is_monotonic() {
		let mut clock = StreamClock::default();
		assert_eq!(clock.micros(Duration::from_secs(100)), 0);
		assert_eq!(clock.micros(Duration::from_millis(100_033)), 33_000);
		assert_eq!(clock.micros(Duration::from_millis(100_033)), 33_001);
		assert_eq!(clock.micros(Duration::from_secs(99)), 33_002);
	}
}
