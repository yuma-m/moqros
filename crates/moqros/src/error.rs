/// Errors produced by moqros.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	/// A pixel format moqros can't convert, e.g. a ROS image encoding it doesn't know.
	#[error("unsupported image encoding: {0}")]
	UnsupportedEncoding(String),

	#[error("invalid image: {0}")]
	/// An image whose dimensions, step or buffer size don't add up.
	InvalidImage(String),

	/// Settings a codec or track rejects, or an unknown codec, format or mode name.
	#[error("invalid settings: {0}")]
	InvalidSettings(String),

	/// An encoder or decoder failed.
	#[error("codec error: {0}")]
	Codec(String),

	/// The codec isn't compiled into this build or its library can't be loaded.
	#[error("codec unavailable: {0}")]
	CodecUnavailable(String),

	/// A decoded picture is larger than [`Decoder::max_pixels`](crate::Decoder::max_pixels).
	#[error("decoded picture {width}x{height} exceeds the limit of {max} pixels")]
	TooLarge {
		/// Picture width in pixels.
		width: u32,
		/// Picture height in pixels.
		height: u32,
		/// The configured limit.
		max: u64,
	},

	/// A MoQ track, group or broadcast failed.
	#[error("moq error: {0}")]
	Moq(#[from] hang::moq_net::Error),

	/// Connecting to the relay failed.
	#[error("moq connection error: {0}")]
	Connection(#[from] moq_tokio::Error),

	/// Reading or writing the hang catalog or a container failed.
	#[error("hang error: {0}")]
	Hang(#[from] hang::Error),

	/// A timestamp doesn't fit the wire format.
	#[error("timestamp out of range")]
	Timestamp,

	/// The broadcast's catalog has no video track.
	#[error("broadcast {0:?} has no video track in its catalog")]
	NoVideo(String),

	/// The broadcast's video renditions all use codecs this build can't decode.
	#[error("no decodable video rendition; broadcast offers: {0}")]
	UnsupportedCodec(String),

	/// The broadcast's catalog has no data track in a format moqros reads.
	#[error("broadcast {0:?} has no data track moqros can read in its catalog")]
	NoData(String),

	/// The broadcast's catalog has neither decodable video nor a data track.
	#[error("broadcast {0:?} has neither decodable video nor a data track")]
	NoTracks(String),

	/// A data track bridged to ROS doesn't say which message type it carries.
	#[error("data track of broadcast {0:?} doesn't name its ROS message type")]
	MissingRosType(String),

	/// A JSON payload or catalog entry is malformed.
	#[error("json error: {0}")]
	Json(#[from] serde_json::Error),

	/// The relay connection closed while waiting for the broadcast.
	#[error("origin closed before broadcast {0:?} was announced")]
	NotAnnounced(String),

	/// An r2r call failed.
	#[cfg(feature = "ros")]
	#[cfg_attr(docsrs, doc(cfg(feature = "ros")))]
	#[error("ros error: {0}")]
	Ros(#[from] r2r::Error),

	/// Publishers of a ROS topic disagree on its message type.
	#[error("{topic} has several types: {}", types.join(", "))]
	TopicTypes {
		/// The fully qualified topic name.
		topic: String,
		/// Every type its publishers announce.
		types: Vec<String>,
	},

	/// A resubscribed broadcast carries something its ROS topic can't take, e.g. data
	/// instead of video, or another message type.
	#[error("broadcast {broadcast} changed from {from} to {to}")]
	BroadcastChanged {
		/// Broadcast name.
		broadcast: String,
		/// What the broadcast carried before.
		from: String,
		/// What it carries now.
		to: String,
	},
}

/// [`std::result::Result`] defaulting to moqros's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;
