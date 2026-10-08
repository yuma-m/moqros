/// Errors produced by moqros.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
	#[error("unsupported image encoding: {0}")]
	UnsupportedEncoding(String),

	#[error("invalid image: {0}")]
	InvalidImage(String),

	#[error("codec error: {0}")]
	Codec(String),

	#[error("codec unavailable: {0}")]
	CodecUnavailable(String),

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

	#[error("origin closed before broadcast {0:?} was announced")]
	NotAnnounced(String),

	#[error("ros error: {0}")]
	Ros(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
