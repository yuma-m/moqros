//! What the ROS graph says about topics.

use std::time::Duration;

use super::ros_error;
use crate::{Error, Result};

/// The message type of `topic` in the ROS graph, or `None` if nothing publishes it yet.
///
/// Relative names resolve against the node's namespace; remappings aren't applied.
/// Fails if publishers disagree on the type.
pub fn topic_type(node: &r2r::Node, topic: &str) -> Result<Option<String>> {
	let namespace = node.namespace().map_err(ros_error)?;
	let node_name = node.fully_qualified_name().map_err(ros_error)?;
	let topic = resolve_topic(&namespace, &node_name, topic);
	let topics = node.get_topic_names_and_types().map_err(ros_error)?;
	match topics.get(&topic).map(Vec::as_slice) {
		None | Some([]) => Ok(None),
		Some([ros_type]) => Ok(Some(ros_type.clone())),
		Some(types) => Err(Error::Ros(format!("{topic} has several types: {}", types.join(", ")))),
	}
}

/// Wait until `topic` shows up in the ROS graph and return its message type (see [`topic_type`]).
pub async fn wait_for_topic_type(node: &r2r::Node, topic: &str) -> Result<String> {
	let mut logged = false;
	loop {
		if let Some(ros_type) = topic_type(node, topic)? {
			return Ok(ros_type);
		}
		if !logged {
			tracing::info!(topic, "waiting for the topic to appear to learn its type");
			logged = true;
		}
		tokio::time::sleep(Duration::from_millis(500)).await;
	}
}

/// Expand a relative or private (`~`) topic name the way rcl does, minus remapping.
fn resolve_topic(namespace: &str, node_name: &str, topic: &str) -> String {
	let join = |base: &str, rest: &str| format!("{}/{}", base.trim_end_matches('/'), rest.trim_start_matches('/'));
	if topic.starts_with('/') {
		topic.to_string()
	} else if let Some(rest) = topic.strip_prefix('~') {
		join(node_name, rest)
	} else {
		join(namespace, topic)
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn resolves_topic_names() {
		for (namespace, topic, resolved) in [
			("/", "/imu", "/imu"),
			("/", "imu", "/imu"),
			("/robot", "imu", "/robot/imu"),
			("/robot", "/imu", "/imu"),
			("/robot", "~/state", "/robot/moqros_pub/state"),
		] {
			let node = format!("{}/moqros_pub", namespace.trim_end_matches('/'));
			assert_eq!(resolve_topic(namespace, &node, topic), resolved, "{namespace} {topic}");
		}
	}
}
