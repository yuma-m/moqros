//! Relay connection settings shared by publishers and subscribers.

use url::Url;

use crate::Result;

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
}

impl ClientConfig {
	pub fn new(url: Url) -> Self {
		Self { url, insecure: false }
	}

	pub(crate) fn client(&self) -> Result<moq_tokio::Client> {
		let mut connect = moq_tokio::connect::Config::default();
		if self.insecure {
			connect.tls.insecure = Some(true);
		}
		Ok(connect.init(Default::default())?)
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
}
