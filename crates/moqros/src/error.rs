/// Errors produced by moqros.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	#[error("unsupported image encoding: {0}")]
	UnsupportedEncoding(String),

	#[error("invalid image: {0}")]
	InvalidImage(String),

	#[error("invalid settings: {0}")]
	InvalidSettings(String),

	#[error("codec error: {0}")]
	Codec(String),

	#[error("codec unavailable: {0}")]
	CodecUnavailable(String),
	#[error("decoded picture {width}x{height} exceeds the limit of {max} pixels")]
	TooLarge { width: u32, height: u32, max: u64 },

	#[error("moq error: {0}")]
	Moq(#[from] hang::moq_net::Error),

	#[error("moq connection error: {0}")]
	Connection(#[from] moq_tokio::Error),

	#[error("hang error: {0}")]
	Hang(#[from] hang::Error),

	#[error("timestamp out of range")]
	Timestamp,

	#[error("broadcast {0:?} has no video track in its catalog")]
	NoVideo(String),

	#[error("no decodable video rendition; broadcast offers: {0}")]
	UnsupportedCodec(String),

	#[error("broadcast {0:?} has no data track moqros can read in its catalog")]
	NoData(String),

	#[error("broadcast {0:?} has neither decodable video nor a data track")]
	NoTracks(String),

	#[error("data track of broadcast {0:?} doesn't name its ROS message type")]
	MissingRosType(String),

	#[error("json error: {0}")]
	Json(#[from] serde_json::Error),

	#[error("origin closed before broadcast {0:?} was announced")]
	NotAnnounced(String),

	#[cfg(feature = "ros")]
	#[cfg_attr(docsrs, doc(cfg(feature = "ros")))]
	#[error("ros error: {0}")]
	Ros(#[from] r2r::Error),

	/// Publishers of a ROS topic disagree on its message type.
	#[error("{topic} has several types: {}", types.join(", "))]
	TopicTypes { topic: String, types: Vec<String> },

	/// A resubscribed broadcast carries something its ROS topic can't take, e.g. data
	/// instead of video, or another message type.
	#[error("broadcast {broadcast} changed from {from} to {to}")]
	BroadcastChanged {
		broadcast: String,
		from: String,
		to: String,
	},
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
