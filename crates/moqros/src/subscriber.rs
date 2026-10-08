//! Subscribe to moqros (or any hang H.264) broadcasts and decode them back into images.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::Catalog;
use hang::moq_net;

use crate::codec::{Codec, Decoder};
use crate::{ClientConfig, Error, Image, Result};

/// A connection to a relay used to consume image broadcasts.
pub struct Subscriber {
	origin: moq_net::origin::Producer,
	connection: moq_tokio::Connection,
}

impl Subscriber {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config
			.client()?
			.with_subscriber(origin.clone())
			.connect(config.url.clone());
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

	/// Wait for `name` to be announced, then subscribe to its best video rendition
	/// whose codec this build can decode.
	pub async fn subscribe_images(&self, name: &str) -> Result<ImageSubscriber> {
		let consumer = self.origin.consume();
		let mut announced = consumer.announced();
		loop {
			let update = announced
				.next()
				.await
				.ok_or_else(|| Error::NotAnnounced(name.to_string()))?;
			if update.kind.is_active() && update.prefix.as_str() == name {
				break;
			}
		}

		let broadcast = consumer.request_broadcast(name).await?;
		let mut catalog = Catalog::<()>::subscribe(&broadcast).await?;
		let catalog = catalog.next().await?.ok_or_else(|| Error::NoVideo(name.to_string()))?;

		let mut renditions = catalog.video.ranked().peekable();
		if renditions.peek().is_none() {
			return Err(Error::NoVideo(name.to_string()));
		}
		let mut offered = Vec::new();
		let (track, config, codec) = renditions
			.find_map(|(track, config)| {
				offered.push(config.codec.to_string());
				let codec = Codec::from_catalog(&config.codec).filter(|c| c.is_available())?;
				Some((track, config, codec))
			})
			.ok_or_else(|| Error::UnsupportedCodec(offered.join(", ")))?;
		tracing::info!(
			broadcast = name,
			track = %track,
			codec = %config.codec,
			width = ?config.coded_width,
			height = ?config.coded_height,
			"subscribing"
		);

		let video = broadcast
			.track(track)?
			.subscribe(moq_net::track::Subscription::default())
			.await?
			.ordered();

		Ok(ImageSubscriber {
			_broadcast: broadcast,
			video,
			group: None,
			codec,
			decoder: Decoder::new(codec)?,
		})
	}
}

/// One encoded video frame received from a broadcast.
#[derive(Debug, Clone)]
pub struct VideoFrame {
	/// Presentation time relative to the start of the broadcast.
	pub timestamp: Duration,
	pub keyframe: bool,
	/// Codec payload (VP8 frame or Annex-B H.264 access unit).
	pub data: Bytes,
}

/// Reads frames of one video track, always jumping to the newest group so a slow
/// reader falls back to the live edge instead of accumulating latency.
pub struct ImageSubscriber {
	_broadcast: moq_net::broadcast::Consumer,
	video: moq_net::track::Ordered,
	group: Option<moq_net::group::Consumer>,
	codec: Codec,
	decoder: Decoder,
}

impl ImageSubscriber {
	/// Codec of the subscribed track.
	pub fn codec(&self) -> Codec {
		self.codec
	}

	/// Next encoded frame, or `None` once the broadcast ends.
	pub async fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
		loop {
			let Some(group) = self.group.as_mut() else {
				match self.video.next_group().await? {
					Some(group) => self.group = Some(group),
					None => return Ok(None),
				}
				continue;
			};

			tokio::select! {
				biased;
				// Prefer a newer group: it starts with a keyframe, so we can drop the rest of this one.
				next = self.video.next_group() => match next? {
					Some(group) => self.group = Some(group),
					None => return Ok(None),
				},
				frame = group.read_frame() => {
					match frame {
						Ok(Some(frame)) => {
							let frame = hang::container::Frame::decode(frame.payload)?;
							return Ok(Some(VideoFrame {
								timestamp: Duration::from_micros(frame.timestamp.as_micros() as u64),
								keyframe: self.codec.is_keyframe(&frame.payload),
								data: frame.payload,
							}));
						}
						Ok(None) => self.group = None,
						Err(err) => {
							tracing::debug!(%err, "group ended early, waiting for the next one");
							self.group = None;
						}
					}
				}
			}
		}
	}

	/// Next decoded `rgb8` image, or `None` once the broadcast ends.
	pub async fn next_image(&mut self) -> Result<Option<Image>> {
		while let Some(frame) = self.next_frame().await? {
			if let Some(image) = self.decoder.decode(&frame.data, frame.timestamp)? {
				return Ok(Some(image));
			}
		}
		Ok(None)
	}
}
