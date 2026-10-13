//! Relay connection settings shared by publishers and subscribers.

use std::path::PathBuf;
use std::time::Duration;

use url::Url;

use crate::{Publisher, Result, Subscriber};

/// Where and how to connect to a MoQ relay.
#[derive(Debug, Clone)]
pub struct ClientConfig {
	/// Relay URL. The path scopes broadcast names, e.g. `http://localhost:4443/anon`.
	///
	/// `http://` fetches the relay's self-signed certificate fingerprint from
	/// `/certificate.sha256` (development only); `https://` verifies TLS normally.
	pub url: Url,
	/// Skip TLS verification entirely. Development only.
	pub insecure: bool,
	/// PEM client certificate chain for mTLS. Set it together with `tls_key`.
	pub tls_cert: Option<PathBuf>,
	/// PEM private key of `tls_cert`.
	pub tls_key: Option<PathBuf>,
}

impl ClientConfig {
	pub fn new(url: Url) -> Self {
		Self {
			url,
			insecure: false,
			tls_cert: None,
			tls_key: None,
		}
	}

	/// Present a client certificate to the relay (mTLS).
	pub fn with_client_cert(mut self, cert: impl Into<PathBuf>, key: impl Into<PathBuf>) -> Self {
		self.tls_cert = Some(cert.into());
		self.tls_key = Some(key.into());
		self
	}

	pub(crate) fn client(&self) -> Result<moq_tokio::Client> {
		let mut connect = moq_tokio::connect::Config::default();
		if self.insecure {
			connect.tls.insecure = Some(true);
		}
		connect.tls.cert = self.tls_cert.clone();
		connect.tls.key = self.tls_key.clone();
		Ok(connect.init(Default::default())?)
	}
}

/// Start one connection to the relay that both publishes and subscribes, e.g. for a robot
/// that streams its camera and takes commands. Must be called inside a tokio runtime.
///
/// The two handles share the session: [`Publisher::closed`] and [`Subscriber::closed`] resolve
/// together, and closing either closes both. Each direction keeps its own origin, so the
/// broadcasts subscribed to aren't published back to the relay.
pub fn connect_both(config: &ClientConfig) -> Result<(Publisher, Subscriber)> {
	let publish = moq_tokio::origin::spawn();
	let subscribe = moq_tokio::origin::spawn();
	let connection = config
		.client()?
		.with_publisher(&publish)
		.with_subscriber(subscribe.clone())
		.connect(config.url.clone());
	Ok((
		Publisher::from_parts(publish, connection.clone()),
		Subscriber::from_parts(subscribe, connection),
	))
}

/// How long [`established`] waits for the relay to refuse a session it just accepted.
const REFUSAL_WINDOW: Duration = Duration::from_millis(500);

/// Wait until a session with the relay is up and the relay hasn't refused it.
///
/// A relay accepts the transport before it asks its auth server about the session, so
/// a refusal (e.g. `Unauthorized`) closes a session that already looked established.
/// Watch the connection for a moment longer so that refusal is returned here.
pub(crate) async fn established(connection: &moq_tokio::Connection) -> Result<()> {
	// The returned handle is a clone; the caller's handle keeps the dial alive.
	let _ = connection.clone().established().await?;
	tokio::select! {
		res = connection.closed() => {
			res?;
			Err(moq_tokio::Error::Reconnect("the connection closed right after it was established".into()).into())
		}
		_ = tokio::time::sleep(REFUSAL_WINDOW) => Ok(()),
	}
}

/// Turn a ROS topic into a broadcast name: `/camera/image_raw` -> `camera/image_raw`.
pub fn broadcast_name_for_topic(topic: &str) -> String {
	topic.trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn topic_to_broadcast_name() {
		assert_eq!(broadcast_name_for_topic("/camera/image_raw"), "camera/image_raw");
		assert_eq!(broadcast_name_for_topic("image"), "image");
	}

	#[test]
	fn broadcast_name_trims_all_outer_slashes() {
		assert_eq!(broadcast_name_for_topic("/front/image_raw/"), "front/image_raw");
		assert_eq!(broadcast_name_for_topic("//image"), "image");
		assert_eq!(broadcast_name_for_topic("/"), "");
	}

	#[test]
	fn new_config_verifies_tls() {
		let config = ClientConfig::new("https://relay.example/anon".parse().unwrap());
		assert!(!config.insecure);
		assert_eq!(config.url.path(), "/anon");
	}

	#[test]
	fn client_cert_needs_a_key() {
		let mut config = ClientConfig::new("https://relay.example/anon".parse().unwrap());
		config.tls_cert = Some("client.pem".into());
		assert!(config.client().is_err());
	}
}
