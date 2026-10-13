//! Subscribe to moqros broadcasts: hang VP8/H.264 video decoded back into images, or data tracks.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::Catalog;
use hang::moq_net;

use crate::codec::{Codec, Decoder};
use crate::data::{DataMode, DataSettings, DataSubscriber};
use crate::{ClientConfig, Error, Image, Result};

/// A connection to a relay used to consume image and data broadcasts.
///
/// Clones share the connection, which stays up while any clone is alive.
#[derive(Clone)]
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
	///
	/// Fails if the relay refuses the session, e.g. for missing credentials.
	pub async fn established(&self) -> Result<()> {
		crate::client::established(&self.connection).await
	}

	/// Resolves when the connection gives up for good.
	pub async fn closed(&self) -> Result<()> {
		Ok(self.connection.closed().await?)
	}

	/// Close the connection for every clone, without redialing.
	///
	/// Data already queued is delivered first, for at most about a second. Call this before
	/// exiting so the relay ends this session's subscriptions right away; otherwise it holds them
	/// until the session times out.
	pub async fn close(&self) -> Result<()> {
		Ok(self.connection.clone().close().await?)
	}

	/// Wait for `name` to be announced, then subscribe to its best video rendition
	/// whose codec this build can decode.
	pub async fn subscribe_images(&self, name: &str) -> Result<ImageSubscriber> {
		let (broadcast, catalog, route) = self.catalog(name).await?;
		images(name, broadcast, &catalog, route).await
	}

	/// Wait for `name` to be announced, then subscribe to its data track.
	pub async fn subscribe_data(&self, name: &str) -> Result<DataSubscriber> {
		let (broadcast, catalog, route) = self.catalog(name).await?;
		data(name, broadcast, &catalog, route).await
	}

	/// Wait for `name` to be announced, then subscribe to whatever it carries: its video
	/// if this build can decode it, otherwise its data track.
	pub async fn subscribe(&self, name: &str) -> Result<Subscription> {
		let (broadcast, catalog, route) = self.catalog(name).await?;
		match Kind::of(&catalog) {
			Kind::Images => Ok(Subscription::Images(images(name, broadcast, &catalog, route).await?)),
			Kind::Data => Ok(Subscription::Data(data(name, broadcast, &catalog, route).await?)),
			Kind::None => Err(Error::NoTracks(name.to_string())),
		}
	}

	/// Wait for `name` to be announced and read its first catalog. Also returns a watch on
	/// the announcement, which the subscription ends with.
	async fn catalog(&self, name: &str) -> Result<(moq_net::broadcast::Consumer, Catalog, RouteWatch)> {
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
		let catalog = catalog.next().await?.ok_or_else(|| Error::NoTracks(name.to_string()))?;
		let route = RouteWatch {
			announced,
			name: name.to_string(),
		};
		Ok((broadcast, catalog, route))
	}
}

async fn images(
	name: &str,
	broadcast: moq_net::broadcast::Consumer,
	catalog: &Catalog,
	route: RouteWatch,
) -> Result<ImageSubscriber> {
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

	let video = GroupReader::subscribe(&broadcast, track, true, route).await?;
	Ok(ImageSubscriber {
		_broadcast: broadcast,
		video,
		codec,
		decoder: Decoder::new(codec)?,
	})
}

async fn data(
	name: &str,
	broadcast: moq_net::broadcast::Consumer,
	catalog: &Catalog,
	route: RouteWatch,
) -> Result<DataSubscriber> {
	let (track, settings) = DataSettings::from_catalog(catalog).ok_or_else(|| Error::NoData(name.to_string()))?;
	tracing::info!(
		broadcast = name,
		track,
		format = %settings.format,
		mode = %settings.mode,
		ros_type = settings.ros_type.as_deref().unwrap_or_default(),
		"subscribing"
	);
	let reader = GroupReader::subscribe(&broadcast, &track, settings.mode == DataMode::Snapshot, route).await?;
	Ok(DataSubscriber::new(broadcast, reader, settings))
}

/// Which track [`Subscriber::subscribe`] reads from a catalog.
#[derive(Debug, PartialEq, Eq)]
enum Kind {
	Images,
	Data,
	None,
}

impl Kind {
	fn of(catalog: &Catalog) -> Self {
		let decodable = catalog
			.video
			.ranked()
			.any(|(_, config)| Codec::from_catalog(&config.codec).is_some_and(Codec::is_available));
		if decodable {
			Self::Images
		} else if DataSettings::from_catalog(catalog).is_some() {
			Self::Data
		} else if !catalog.video.renditions.is_empty() {
			// Subscribing fails, but says which codecs the broadcast offers.
			Self::Images
		} else {
			Self::None
		}
	}
}

/// What [`Subscriber::subscribe`] found in a broadcast.
pub enum Subscription {
	Images(ImageSubscriber),
	Data(DataSubscriber),
}

/// Watches the announcement of one subscribed broadcast.
///
/// A relay doesn't end a subscription when the broadcast's publisher retracts it: the track
/// just goes quiet. Nor does it move the subscription when another publisher of the same name
/// takes over. So a subscription ends on any change to the broadcast's route, and the caller
/// resubscribes to whatever serves the name next.
///
/// That includes [`Updated`](moq_net::announce::Kind::Updated), though moq-net delivers a
/// re-priced route in place: a takeover by another publisher also arrives as `Updated` (the path
/// stays covered throughout), and it can't be told apart from a mere re-pricing. Route changes
/// are rare, so an occasional needless resubscribe is the cheaper mistake.
pub(crate) struct RouteWatch {
	announced: moq_net::announce::Consumer,
	name: String,
}

impl RouteWatch {
	/// Resolves on the next update to this broadcast's announcement: retracted, re-announced,
	/// or re-routed. Never resolves once the origin closes; the track ends then anyway.
	async fn changed(&mut self) -> moq_net::announce::Kind {
		while let Some(update) = self.announced.next().await {
			if update.prefix.as_str() == self.name {
				return update.kind;
			}
		}
		std::future::pending().await
	}
}

/// Reads the frames of one track, group after group, until the track ends or its broadcast's
/// route changes.
pub(crate) struct GroupReader {
	frames: Frames,
	route: RouteWatch,
	ended: bool,
}

impl GroupReader {
	async fn subscribe(
		broadcast: &moq_net::broadcast::Consumer,
		track: &str,
		skip: bool,
		route: RouteWatch,
	) -> Result<Self> {
		let track = broadcast
			.track(track)?
			.subscribe(moq_net::track::Subscription::default())
			.await?
			.ordered();
		Ok(Self {
			frames: Frames {
				track,
				group: None,
				skip,
			},
			route,
			ended: false,
		})
	}

	/// Next frame, or `None` once the track ends or the broadcast's route changes.
	pub(crate) async fn next_frame(&mut self) -> Result<Option<moq_net::frame::Frame>> {
		if self.ended {
			return Ok(None);
		}
		tokio::select! {
			// Frames first: whatever arrived before the route changed (e.g. a last stop command
			// sent just before the retraction) is still delivered.
			biased;
			frame = self.frames.next() => {
				let frame = frame?;
				self.ended = frame.is_none();
				Ok(frame)
			}
			kind = self.route.changed() => {
				tracing::info!(broadcast = self.route.name, ?kind, "broadcast announcement changed, ending the subscription");
				self.ended = true;
				Ok(None)
			}
		}
	}
}

/// The frames of one track, group after group.
struct Frames {
	track: moq_net::track::Ordered,
	group: Option<moq_net::group::Consumer>,
	/// Jump to a newer group as soon as one arrives, dropping the rest of the current one.
	skip: bool,
}

impl Frames {
	/// Next frame, or `None` once the track ends.
	async fn next(&mut self) -> Result<Option<moq_net::frame::Frame>> {
		loop {
			let Some(group) = self.group.as_mut() else {
				match self.track.next_group().await? {
					Some(group) => self.group = Some(group),
					None => return Ok(None),
				}
				continue;
			};

			let frame = if self.skip {
				tokio::select! {
					biased;
					// A newer group starts with a keyframe (or a complete message), so the rest of
					// this one can go.
					next = self.track.next_group() => {
						match next? {
							Some(group) => self.group = Some(group),
							None => return Ok(None),
						}
						continue;
					}
					frame = group.read_frame() => frame,
				}
			} else {
				group.read_frame().await
			};
			match frame {
				Ok(Some(frame)) => return Ok(Some(frame)),
				Ok(None) => self.group = None,
				Err(err) => {
					tracing::debug!(%err, "group ended early, waiting for the next one");
					self.group = None;
				}
			}
		}
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
	video: GroupReader,
	codec: Codec,
	decoder: Decoder,
}

impl ImageSubscriber {
	/// Codec of the subscribed track.
	pub fn codec(&self) -> Codec {
		self.codec
	}

	/// Accept decoded pictures of up to `max_pixels`; see [`Decoder::set_max_pixels`].
	pub fn set_max_pixels(&mut self, max_pixels: u64) {
		self.decoder.set_max_pixels(max_pixels);
	}

	/// Next encoded frame, or `None` once the broadcast ends.
	pub async fn next_frame(&mut self) -> Result<Option<VideoFrame>> {
		let Some(frame) = self.video.next_frame().await? else {
			return Ok(None);
		};
		let frame = hang::container::Frame::decode(frame.payload)?;
		Ok(Some(VideoFrame {
			timestamp: Duration::from_micros(frame.timestamp.as_micros() as u64),
			keyframe: self.codec.is_keyframe(&frame.payload),
			data: frame.payload,
		}))
	}

	/// Next decoded `rgb8` image, or `None` once the broadcast ends.
	///
	/// Fails with [`Error::TooLarge`] for a picture above the decoder's
	/// [`max_pixels`](Decoder::max_pixels).
	pub async fn next_image(&mut self) -> Result<Option<Image>> {
		while let Some(frame) = self.next_frame().await? {
			if let Some(image) = self.decoder.decode(&frame.data, frame.timestamp)? {
				return Ok(Some(image));
			}
		}
		Ok(None)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn catalog(json: serde_json::Value) -> Catalog {
		serde_json::from_value(json).unwrap()
	}

	#[test]
	fn picks_what_this_build_can_read() {
		let video = |codec: &str| serde_json::json!({ "renditions": { "video": { "codec": codec } } });
		let data = serde_json::json!({ "tracks": { "data": { "mode": "snapshot" } } });

		assert_eq!(Kind::of(&catalog(serde_json::json!({}))), Kind::None);
		assert_eq!(Kind::of(&catalog(serde_json::json!({ "json": data }))), Kind::Data);
		// Video nobody can decode still subscribes, to report the codecs on offer...
		assert_eq!(
			Kind::of(&catalog(serde_json::json!({ "video": video("av01.0.04M.08") }))),
			Kind::Images
		);
		// ...unless there's data to read instead.
		assert_eq!(
			Kind::of(&catalog(
				serde_json::json!({ "video": video("av01.0.04M.08"), "json": data })
			)),
			Kind::Data
		);
		for codec in Codec::available() {
			let codec = match codec {
				Codec::Vp8 => "vp8",
				Codec::H264 => "avc3.42001f",
			};
			assert_eq!(
				Kind::of(&catalog(serde_json::json!({ "video": video(codec), "json": data }))),
				Kind::Images,
				"{codec}"
			);
		}
	}
}
