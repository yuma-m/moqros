//! Subscribe to moqros broadcasts: hang VP8/H.264/AV1 video decoded back into images, or data tracks.

use std::time::Duration;

use bytes::Bytes;
use hang::catalog::Catalog;
use hang::moq_net;

use crate::catalog::{self, Kind};
use crate::codec::{Codec, Decoder};
use crate::data::{DataMode, DataSubscriber};
use crate::track::{GroupReader, RouteWatch};
use crate::{ClientConfig, Error, Image, Result, Session};

/// A connection to a relay used to consume image and data broadcasts.
///
/// Clones share the connection, which stays up while any clone is alive.
#[derive(Clone)]
pub struct Subscriber {
	origin: moq_net::origin::Producer,
	session: Session,
}

impl Subscriber {
	/// Start connecting to the relay. Must be called inside a tokio runtime.
	pub fn connect(config: &ClientConfig) -> Result<Self> {
		let origin = moq_tokio::origin::spawn();
		let connection = config
			.client()?
			.with_subscriber(origin.clone())
			.connect(config.url.clone());
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
		let route = RouteWatch::new(announced, name);
		Ok((broadcast, catalog, route))
	}
}

async fn images(
	name: &str,
	broadcast: moq_net::broadcast::Consumer,
	catalog: &Catalog,
	route: RouteWatch,
) -> Result<ImageSubscriber> {
	let (track, config, codec) = catalog::select_video(catalog, name)?;
	tracing::info!(
		broadcast = name,
		// The track name comes from the remote catalog: record it as a str (quoted
		// and escaped), not with `%`, so control characters can't forge log lines.
		track,
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
	let (track, settings) = catalog::find_data(catalog).ok_or_else(|| Error::NoData(name.to_string()))?;
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

/// What [`Subscriber::subscribe`] found in a broadcast.
#[non_exhaustive]
pub enum Subscription {
	/// The broadcast carries decodable video.
	Images(ImageSubscriber),
	/// The broadcast carries a data track.
	Data(DataSubscriber),
}

/// One encoded video frame received from a broadcast.
#[derive(Debug, Clone)]
pub struct VideoFrame {
	/// Presentation time relative to the start of the broadcast.
	pub timestamp: Duration,
	/// Whether the frame decodes on its own.
	pub keyframe: bool,
	/// Codec payload (VP8 frame, Annex-B H.264 access unit, or AV1 temporal unit).
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
			timestamp: Duration::from_micros(u64::try_from(frame.timestamp.as_micros()).unwrap_or(u64::MAX)),
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
