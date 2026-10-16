//! Running the bridges of one node over one relay connection.

use std::time::Duration;

use anyhow::{Context, bail, ensure};
use futures::StreamExt;
use moqros::ros::{MoqToRos, QosProfile, r2r};
use tokio::task::JoinHandle;

use crate::runtime::{SharedNode, Spinner, shutdown_signal, spawn_spinner};
use crate::spec::{PubTopic, SubBroadcast};

/// What a bridge node does: ROS topics to publish to the relay, and broadcasts to republish
/// on ROS.
pub struct Bridges {
	pub topics: Vec<PubTopic>,
	pub broadcasts: Vec<SubBroadcast>,
	/// For video topics.
	pub encoder: moqros::EncoderSettings,
	pub qos: QosProfile,
}

/// The relay connection of a bridge node: a publisher for its topics, a subscriber for its
/// broadcasts, or both over one connection.
pub struct Relay {
	publisher: Option<moqros::Publisher>,
	subscriber: Option<moqros::Subscriber>,
	session: moqros::Session,
}

impl Relay {
	/// For a node that only has topics.
	pub fn publisher(publisher: moqros::Publisher) -> Self {
		Self {
			session: publisher.session().clone(),
			publisher: Some(publisher),
			subscriber: None,
		}
	}

	/// For a node that only has broadcasts.
	pub fn subscriber(subscriber: moqros::Subscriber) -> Self {
		Self {
			session: subscriber.session().clone(),
			publisher: None,
			subscriber: Some(subscriber),
		}
	}

	/// Both directions over the one connection [`moqros::connect_both`] returns.
	pub fn both(publisher: moqros::Publisher, subscriber: moqros::Subscriber) -> Self {
		Self {
			session: publisher.session().clone(),
			publisher: Some(publisher),
			subscriber: Some(subscriber),
		}
	}
}

/// Run `bridges` until one of them ends, the relay connection gives up, or SIGINT or SIGTERM
/// arrives (which closes the connection first, so the relay ends this node's broadcasts and
/// subscriptions right away).
///
/// Video topics start at once. A data topic without a type starts when it appears in the ROS
/// graph. Broadcasts are waited for all at once, each starting when it's announced; one whose
/// topic can't be created (e.g. a message type without bindings) is skipped.
pub async fn run_bridges(node: r2r::Node, relay: Relay, bridges: Bridges) -> anyhow::Result<()> {
	let Bridges {
		topics,
		broadcasts,
		encoder,
		qos,
	} = bridges;
	ensure!(
		topics.is_empty() || relay.publisher.is_some(),
		"topics to publish need a publisher"
	);
	ensure!(
		broadcasts.is_empty() || relay.subscriber.is_some(),
		"broadcasts to republish need a subscriber"
	);

	let node = SharedNode::new(node);
	// Only the ROS -> MoQ side subscribes to anything; a node that only publishes has
	// nothing to spin for.
	let mut spinner = match topics.is_empty() {
		true => Spinner::idle(),
		false => spawn_spinner(node.clone()),
	};

	// A topic's type depends on what its broadcast carries, so wait for each broadcast
	// (all at once) before creating its publisher.
	let mut subscribing: futures::stream::FuturesUnordered<_> = broadcasts
		.into_iter()
		.filter_map(|broadcast| {
			let subscriber = relay.subscriber.clone()?;
			Some(async move { (MoqToRos::subscribe(subscriber, &broadcast.broadcast).await, broadcast) })
		})
		.collect();
	let mut pub_bridges = futures::stream::FuturesUnordered::new();
	let mut sub_bridges = futures::stream::FuturesUnordered::new();
	let mut pending = PendingTopics::new(topics);
	// Created once, so a signal arriving while the loop body runs isn't missed.
	let shutdown = shutdown_signal();
	tokio::pin!(shutdown);

	loop {
		if let Some(publisher) = &relay.publisher {
			for (topic, bridge) in pending.start_ready(&node, publisher, &encoder, &qos)? {
				pub_bridges.push(async move { (topic, bridge.await) });
			}
		}

		tokio::select! {
			Some((ready, broadcast)) = subscribing.next() => {
				match node.with(|node| spawn_sub_broadcast(ready, node, &broadcast, qos.clone())) {
					Ok(bridge) => sub_bridges.push(async move { (broadcast.broadcast, bridge.await) }),
					// E.g. a message type this build has no bindings for: the other
					// broadcasts and topics can still go on.
					Err(err) => {
						tracing::error!(%err, broadcast = broadcast.broadcast, topic = broadcast.topic, "skipping broadcast");
						if subscribing.is_empty() && sub_bridges.is_empty() && pub_bridges.is_empty() && pending.is_empty() {
							bail!("nothing left to bridge: no broadcast could be republished");
						}
					}
				}
			}
			Some((topic, res)) = pub_bridges.next() => {
				let stats = res.context("bridge panicked")??;
				tracing::info!(topic, ?stats, "topic stream ended");
				return Ok(());
			}
			Some((broadcast, res)) = sub_bridges.next() => {
				res.context("bridge panicked")?.with_context(|| format!("broadcast {broadcast}"))?;
				return Ok(());
			}
			_ = tokio::time::sleep(Duration::from_millis(500)), if !pending.is_empty() => {}
			// E.g. the relay refused the session: nothing would ever be announced.
			res = relay.session.closed() => return res.context("relay connection closed"),
			err = spinner.failed() => return Err(err),
			res = &mut shutdown => {
				tracing::info!(signal = res?, "shutting down");
				if let Err(err) = relay.session.close().await {
					tracing::warn!(%err, "the relay connection didn't close cleanly");
				}
				return Ok(());
			}
		}
	}
}

/// A running ROS -> MoQ bridge task.
type PubBridge = JoinHandle<moqros::Result<moqros::ros::BridgeStats>>;

/// Topics to publish that haven't started yet, because their type isn't known.
struct PendingTopics {
	topics: Vec<PubTopic>,
	/// Whether the topics still waiting were logged.
	logged: bool,
}

impl PendingTopics {
	fn new(topics: Vec<PubTopic>) -> Self {
		Self { topics, logged: false }
	}

	fn is_empty(&self) -> bool {
		self.topics.is_empty()
	}

	/// Start every topic whose type is known by now, returning each one's name and bridge.
	/// Data topics without a `type` wait until something publishes them.
	fn start_ready(
		&mut self,
		node: &SharedNode,
		publisher: &moqros::Publisher,
		encoder: &moqros::EncoderSettings,
		qos: &QosProfile,
	) -> anyhow::Result<Vec<(String, PubBridge)>> {
		let mut started = Vec::new();
		let mut waiting = Vec::new();
		for topic in std::mem::take(&mut self.topics) {
			match node.with(|node| start_pub_topic(node, publisher, &topic, encoder, qos.clone()))? {
				Some(bridge) => started.push((topic.topic.clone(), bridge)),
				None => {
					waiting.push(topic.topic.clone());
					self.topics.push(topic);
				}
			}
		}
		if !waiting.is_empty() && !self.logged {
			tracing::info!(topics = ?waiting, "waiting for topics to appear to learn their types");
			self.logged = true;
		}
		Ok(started)
	}
}

/// Start bridging `topic` from ROS to MoQ once its type is known. Returns `None` while
/// the topic hasn't appeared in the ROS graph yet.
fn start_pub_topic(
	node: &mut r2r::Node,
	publisher: &moqros::Publisher,
	topic: &PubTopic,
	encoder: &moqros::EncoderSettings,
	qos: QosProfile,
) -> anyhow::Result<Option<PubBridge>> {
	let bridge = match topic.format.data() {
		None => {
			let images = publisher.create_image_broadcast(&topic.broadcast, encoder.clone())?;
			moqros::ros::spawn_ros_to_moq(node, &topic.topic, qos, images)?
		}
		Some(format) => {
			let ros_type = match &topic.ros_type {
				Some(ros_type) => ros_type.clone(),
				None => match moqros::ros::topic_type(node, &topic.topic)? {
					Some(ros_type) => ros_type,
					None => return Ok(None),
				},
			};
			let settings = moqros::DataSettings {
				format,
				mode: topic.mode,
				ros_type: Some(ros_type),
			};
			let data = publisher.create_data_broadcast(&topic.broadcast, settings)?;
			moqros::ros::spawn_ros_to_moq_data(node, &topic.topic, qos, data)?
		}
	};
	tracing::info!(
		topic = topic.topic,
		format = ?topic.format,
		broadcast = topic.broadcast,
		"bridging ROS -> MoQ"
	);
	Ok(Some(bridge))
}

/// A running MoQ -> ROS bridge task.
type SubBridge = JoinHandle<moqros::Result<()>>;

/// Create the ROS publisher for a subscribed broadcast and start republishing it.
fn spawn_sub_broadcast(
	ready: moqros::ros::MoqToRos,
	node: &mut r2r::Node,
	broadcast: &SubBroadcast,
	qos: QosProfile,
) -> moqros::Result<SubBridge> {
	let target = moqros::ros::RosTarget {
		topic: broadcast.topic.clone(),
		qos,
		frame_id: broadcast.frame_id.clone(),
		ros_type: broadcast.ros_type.clone(),
	};
	let bridge = ready.spawn(node, target)?;
	tracing::info!(
		broadcast = broadcast.broadcast,
		topic = broadcast.topic,
		"bridging MoQ -> ROS"
	);
	Ok(bridge)
}
