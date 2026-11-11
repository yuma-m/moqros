//! Running the bridges of one node over one relay connection.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use anyhow::{Context, bail, ensure};
use futures::StreamExt;
use futures::stream::FuturesUnordered;
use moqros::ros::{BitrateControl, Bridge, MoqToRos, QosProfile, ROS_COMPRESSED_IMAGE_TYPE, r2r};
use tokio::task::JoinError;

use crate::args::Format;
use crate::diagnostics::Diagnostics;
use crate::runtime::{SharedNode, Spinner, shutdown_signal, spawn_spinner};
use crate::spec::{Discovery, PubTopic, SubBroadcast, check_video_type};

/// What a bridge node does: ROS topics to publish to the relay, and broadcasts to republish
/// on ROS.
pub struct Bridges {
	pub topics: Vec<PubTopic>,
	pub broadcasts: Vec<SubBroadcast>,
	/// For video topics.
	pub video: moqros::ImageBroadcastSettings,
	pub qos: QosProfile,
	/// How often to publish the bridges' statistics on `/diagnostics`, if at all.
	pub diagnostics_period: Option<Duration>,
	/// Broadcasts to find by prefix and republish too.
	pub discovery: Option<Discovery>,
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

/// Run `bridges` until the relay connection gives up, the ROS spinner dies, or SIGINT or SIGTERM
/// arrives (which closes the connection first, so the relay ends this node's broadcasts and
/// subscriptions right away).
///
/// A topic without a type starts when it appears in the ROS graph. Broadcasts are waited for all
/// at once, each starting when it's announced; one whose topic can't be created (e.g. a message
/// type without bindings) is skipped. With `discovery`, every broadcast announced under its
/// prefixes is republished too, once.
///
/// A bridge that stops or fails is restarted on its own, after a delay that grows while it
/// keeps failing (up to 30 seconds), so one broken topic or broadcast doesn't take the others
/// down.
pub async fn run_bridges(node: r2r::Node, relay: Relay, bridges: Bridges) -> anyhow::Result<()> {
	let Bridges {
		topics,
		broadcasts,
		mut video,
		qos,
		diagnostics_period,
		discovery,
	} = bridges;
	ensure!(
		topics.is_empty() || relay.publisher.is_some(),
		"topics to publish need a publisher"
	);
	ensure!(
		(broadcasts.is_empty() && discovery.is_none()) || relay.subscriber.is_some(),
		"broadcasts to republish need a subscriber"
	);
	// Broadcasts and topics taken, so a found broadcast doesn't take one again.
	let mut known_broadcasts: HashSet<String> = broadcasts
		.iter()
		.map(|b| moqros::normalize_broadcast_name(&b.broadcast).to_string())
		.collect();
	let mut used_topics: HashSet<String> = broadcasts
		.iter()
		.map(|b| b.topic.clone())
		.chain(topics.iter().map(|t| t.topic.clone()))
		.collect();
	let mut announcements = relay
		.subscriber
		.as_ref()
		.filter(|_| discovery.is_some())
		.map(|subscriber| subscriber.announced(""));

	let node = SharedNode::new(node);
	// Only the ROS -> MoQ side subscribes to anything; a node that only publishes has
	// nothing to spin for.
	let mut spinner = match topics.is_empty() {
		true => Spinner::idle(),
		false => spawn_spinner(node.clone()),
	};

	// A topic's type depends on what its broadcast carries, so wait for each broadcast
	// (all at once) before creating its publisher.
	let mut subscribing: FuturesUnordered<_> = broadcasts
		.into_iter()
		.filter_map(|broadcast| Some(subscribe_after(relay.subscriber.clone()?, broadcast, Duration::ZERO)))
		.collect();
	let mut pub_bridges = FuturesUnordered::new();
	let mut sub_bridges = FuturesUnordered::new();
	let has_video = topics.iter().any(|topic| topic.format == Format::Video);
	let mut pending = PendingTopics::new(topics);
	let mut restarts = Restarts::default();
	// Parameter changes need the spinner, which only runs with topics; only video topics take any.
	let mut parameters = match has_video {
		true => Some(node.with(watch_parameters)?),
		false => None,
	};
	let mut bitrates: HashMap<String, BitrateControl> = HashMap::new();
	let mut diagnostics = diagnostics_period.and_then(|period| match node.with(Diagnostics::new) {
		Ok(diagnostics) => Some((diagnostics, tokio::time::interval(period))),
		Err(err) => {
			tracing::warn!(%err, "not publishing diagnostics; build with diagnostic_msgs in IDL_PACKAGE_FILTER");
			None
		}
	});
	// Created once, so a signal arriving while the loop body runs isn't missed.
	let shutdown = shutdown_signal();
	tokio::pin!(shutdown);

	loop {
		if let Some(publisher) = &relay.publisher {
			for (topic, bridge) in pending.start_ready(&node, publisher, &video, &qos, &mut restarts)? {
				let stats = bridge.watch_stats();
				if let Some((diagnostics, _)) = &mut diagnostics {
					diagnostics.running(&topic.topic, &format!("ROS -> MoQ {}", topic.format), stats.clone());
				}
				if let Some(control) = bridge.bitrate_control() {
					bitrates.insert(topic.topic.clone(), control);
				}
				pub_bridges.push(async move { (topic, stats, bridge.await) });
			}
		}

		tokio::select! {
			Some((ready, broadcast)) = subscribing.next() => {
				match node.with(|node| spawn_sub_broadcast(ready, node, &broadcast, qos.clone())) {
					Ok(mut bridge) => {
						let stats = bridge.watch_stats();
						if let Some((diagnostics, _)) = &mut diagnostics {
							diagnostics.running(&broadcast.broadcast, "MoQ -> ROS", stats.clone());
						}
						let teardown = bridge.take_teardown();
						sub_bridges.push(async move { (broadcast, stats, teardown, bridge.await) });
					}
					// E.g. a message type this build has no bindings for: the other
					// broadcasts and topics can still go on.
					Err(err) => {
						tracing::error!(%err, broadcast = broadcast.broadcast, topic = broadcast.topic, "skipping broadcast");
						if discovery.is_none() && subscribing.is_empty() && sub_bridges.is_empty() && pub_bridges.is_empty() && pending.is_empty() {
							bail!("nothing left to bridge: no broadcast could be republished");
						}
					}
				}
			}
			Some(name) = next_announcement(&mut announcements) => {
				let Some(discovery) = &discovery else { continue };
				if !discovery.covers(&name) || !known_broadcasts.insert(moqros::normalize_broadcast_name(&name).to_string()) {
					continue;
				}
				match discovery.broadcast(&name) {
					Ok(broadcast) if used_topics.insert(broadcast.topic.clone()) => {
						tracing::info!(broadcast = name, topic = broadcast.topic, "found a broadcast");
						if let Some(subscriber) = relay.subscriber.clone() {
							subscribing.push(subscribe_after(subscriber, broadcast, Duration::ZERO));
						}
					}
					Ok(broadcast) => tracing::warn!(broadcast = name, topic = broadcast.topic, "skipping a found broadcast: its topic is taken"),
					Err(err) => tracing::warn!(broadcast = name, %err, "skipping a found broadcast"),
				}
			}
			Some((name, value)) = next_parameter(&mut parameters) => {
				match runtime_bitrate(&name, &value) {
					Some(Ok(bitrate)) => {
						tracing::info!(bitrate, "changing the video bitrate");
						video.encoder.bitrate = bitrate;
						for control in bitrates.values() {
							control.set(bitrate);
						}
					}
					Some(Err(err)) => tracing::warn!(%err, "ignoring the new bitrate"),
					None => tracing::warn!(parameter = name, "only `bitrate` changes at runtime; restart the node to apply this parameter"),
				}
			}
			Some((topic, stats, res)) = pub_bridges.next() => {
				let stats = *stats.borrow();
				log_stopped("topic", &topic.topic, &stats, res);
				if let Some((diagnostics, _)) = &mut diagnostics {
					diagnostics.restarting(&topic.topic);
				}
				let delay = restarts.next(&format!("topic {}", topic.topic), stats.sent > 0);
				tracing::info!(topic = topic.topic, ?delay, "restarting the topic bridge");
				pending.restart(topic, delay);
			}
			Some((broadcast, stats, teardown, res)) = sub_bridges.next() => {
				let stats = *stats.borrow();
				log_stopped("broadcast", &broadcast.broadcast, &stats, res);
				if let Some((diagnostics, _)) = &mut diagnostics {
					diagnostics.restarting(&broadcast.broadcast);
				}
				node.with(|node| teardown.run(node));
				let delay = restarts.next(&format!("broadcast {}", broadcast.broadcast), stats.sent > 0);
				tracing::info!(broadcast = broadcast.broadcast, ?delay, "restarting the broadcast bridge");
				if let Some(subscriber) = relay.subscriber.clone() {
					subscribing.push(subscribe_after(subscriber, broadcast, delay));
				}
			}
			_ = tokio::time::sleep(Duration::from_millis(500)), if !pending.is_empty() => {}
			Some(diagnostics) = next_report(&mut diagnostics) => {
				if let Err(err) = diagnostics.publish() {
					tracing::warn!(%err, "publishing diagnostics failed");
				}
			}
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

/// Changes to the node's ROS parameters, through its `set_parameters` service.
type ParameterChanges = std::pin::Pin<Box<dyn futures::Stream<Item = (String, r2r::ParameterValue)> + Send>>;

/// Serve the node's parameter services, returning the changes they make.
fn watch_parameters(node: &mut r2r::Node) -> anyhow::Result<ParameterChanges> {
	let (serve, changes) = node
		.make_parameter_handler()
		.context("failed to serve ROS parameters")?;
	tokio::spawn(serve);
	Ok(Box::pin(changes))
}

/// The next broadcast announced. Never resolves without discovery.
async fn next_announcement(announcements: &mut Option<moqros::Announcements>) -> Option<String> {
	match announcements {
		Some(announcements) => announcements.next().await,
		None => std::future::pending().await,
	}
}

/// The next parameter change. Never resolves without parameter services.
async fn next_parameter(parameters: &mut Option<ParameterChanges>) -> Option<(String, r2r::ParameterValue)> {
	match parameters {
		Some(changes) => changes.next().await,
		None => std::future::pending().await,
	}
}

/// The new bitrate if parameter `name` is the bitrate, or why `value` isn't one; `None` for other
/// parameters.
fn runtime_bitrate(name: &str, value: &r2r::ParameterValue) -> Option<Result<u32, String>> {
	(name == "bitrate").then(|| match value {
		r2r::ParameterValue::Integer(bitrate) => u32::try_from(*bitrate)
			.ok()
			.filter(|&bitrate| bitrate > 0)
			.ok_or_else(|| format!("{bitrate} is not a bitrate in bit/s")),
		other => Err(format!("{other:?} is not an integer")),
	})
}

/// The diagnostics, once their next report is due. Never resolves without diagnostics.
async fn next_report(diagnostics: &mut Option<(Diagnostics, tokio::time::Interval)>) -> Option<&mut Diagnostics> {
	match diagnostics {
		Some((diagnostics, interval)) => {
			interval.tick().await;
			Some(diagnostics)
		}
		None => std::future::pending().await,
	}
}

/// Wait `delay`, then subscribe to `broadcast` (retrying until it's announced).
async fn subscribe_after(
	subscriber: moqros::Subscriber,
	broadcast: SubBroadcast,
	delay: Duration,
) -> (MoqToRos, SubBroadcast) {
	tokio::time::sleep(delay).await;
	(
		MoqToRos::subscribe_with(subscriber, &broadcast.broadcast, broadcast.image_options()).await,
		broadcast,
	)
}

/// Log why the bridge of `kind` `name` stopped.
fn log_stopped(
	kind: &str,
	name: &str,
	stats: &moqros::ros::BridgeStats,
	res: Result<moqros::Result<moqros::ros::BridgeStats>, JoinError>,
) {
	match res {
		Ok(Ok(_)) => tracing::warn!(kind, name, ?stats, "bridge stopped"),
		Ok(Err(err)) => tracing::warn!(kind, name, ?stats, %err, "bridge failed"),
		Err(err) => tracing::error!(kind, name, ?stats, %err, "bridge panicked"),
	}
}

/// How long to wait before restarting each bridge that stopped: one second at first, doubling
/// up to [`Restarts::MAX`] while it keeps stopping without getting a message through.
#[derive(Debug, Default)]
struct Restarts {
	delays: HashMap<String, Duration>,
}

impl Restarts {
	const MIN: Duration = Duration::from_secs(1);
	const MAX: Duration = Duration::from_secs(30);

	/// The delay before restarting `bridge`, which stopped after sending a message if
	/// `progressed`.
	fn next(&mut self, bridge: &str, progressed: bool) -> Duration {
		let delay = self.delays.entry(bridge.to_string()).or_default();
		*delay = match progressed || delay.is_zero() {
			true => Self::MIN,
			false => (*delay * 2).min(Self::MAX),
		};
		*delay
	}
}

/// A topic to publish that hasn't started (again) yet.
struct Pending {
	topic: PubTopic,
	/// Not started before this.
	due: Instant,
	/// Whether it ran before, so a failure to start is retried instead of fatal.
	restart: bool,
}

/// Topics to publish that haven't started yet, because their type isn't known or they wait
/// to be restarted.
struct PendingTopics {
	topics: Vec<Pending>,
	/// Whether the topics still waiting for their type were logged.
	logged: bool,
}

impl PendingTopics {
	fn new(topics: Vec<PubTopic>) -> Self {
		let now = Instant::now();
		let topics = topics
			.into_iter()
			.map(|topic| Pending {
				topic,
				due: now,
				restart: false,
			})
			.collect();
		Self { topics, logged: false }
	}

	fn is_empty(&self) -> bool {
		self.topics.is_empty()
	}

	/// Start `topic` again in `delay`.
	fn restart(&mut self, topic: PubTopic, delay: Duration) {
		self.topics.push(Pending {
			topic,
			due: Instant::now() + delay,
			restart: true,
		});
	}

	/// Start every topic that is due and whose type is known by now, returning each one with
	/// its bridge. Data topics without a `type` wait until something publishes them.
	///
	/// A topic that fails to start fails the node the first time (it's misconfigured), and is
	/// retried later on a restart.
	fn start_ready(
		&mut self,
		node: &SharedNode,
		publisher: &moqros::Publisher,
		video: &moqros::ImageBroadcastSettings,
		qos: &QosProfile,
		restarts: &mut Restarts,
	) -> anyhow::Result<Vec<(PubTopic, Bridge)>> {
		let now = Instant::now();
		let mut started = Vec::new();
		let mut waiting = Vec::new();
		for mut pending in std::mem::take(&mut self.topics) {
			if pending.due > now {
				self.topics.push(pending);
				continue;
			}
			match node.with(|node| start_pub_topic(node, publisher, &pending.topic, video, qos.clone())) {
				Ok(Some(bridge)) => started.push((pending.topic, bridge)),
				Ok(None) => {
					waiting.push(pending.topic.topic.clone());
					self.topics.push(pending);
				}
				Err(err) if pending.restart => {
					let delay = restarts.next(&format!("topic {}", pending.topic.topic), false);
					tracing::warn!(topic = pending.topic.topic, %err, ?delay, "restarting the topic bridge failed");
					pending.due = now + delay;
					self.topics.push(pending);
				}
				Err(err) => return Err(err),
			}
		}
		if !waiting.is_empty() && !self.logged {
			tracing::info!(topics = ?waiting, "waiting for topics to appear to learn their types");
			self.logged = true;
		}
		Ok(started)
	}
}

/// Start bridging `topic` from ROS to MoQ once its type is known: given, or looked up in the ROS
/// graph. Returns `None` while the topic hasn't appeared there yet.
fn start_pub_topic(
	node: &mut r2r::Node,
	publisher: &moqros::Publisher,
	topic: &PubTopic,
	video: &moqros::ImageBroadcastSettings,
	qos: QosProfile,
) -> anyhow::Result<Option<Bridge>> {
	let ros_type = match &topic.ros_type {
		Some(ros_type) => ros_type.clone(),
		None => match moqros::ros::topic_type(node, &topic.topic)? {
			Some(ros_type) => ros_type,
			None => return Ok(None),
		},
	};
	let bridge = match topic.format.data() {
		None => {
			check_video_type(&topic.topic, &ros_type)?;
			let images = publisher.create_image_broadcast(&topic.broadcast, video.clone())?;
			match ros_type.as_str() {
				ROS_COMPRESSED_IMAGE_TYPE => moqros::ros::spawn_ros_to_moq_compressed(node, &topic.topic, qos, images)?,
				_ => moqros::ros::spawn_ros_to_moq(node, &topic.topic, qos, images)?,
			}
		}
		Some(format) => {
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

/// Create the ROS publisher for a subscribed broadcast and start republishing it.
fn spawn_sub_broadcast(
	ready: moqros::ros::MoqToRos,
	node: &mut r2r::Node,
	broadcast: &SubBroadcast,
	qos: QosProfile,
) -> moqros::Result<Bridge> {
	let bridge = ready.spawn(node, broadcast.target(qos))?;
	tracing::info!(
		broadcast = broadcast.broadcast,
		topic = broadcast.topic,
		"bridging MoQ -> ROS"
	);
	Ok(bridge)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn reads_runtime_bitrates() {
		use r2r::ParameterValue::{Double, Integer};
		assert_eq!(runtime_bitrate("bitrate", &Integer(500_000)), Some(Ok(500_000)));
		assert!(matches!(runtime_bitrate("bitrate", &Integer(0)), Some(Err(_))));
		assert!(matches!(runtime_bitrate("bitrate", &Integer(-1)), Some(Err(_))));
		assert!(matches!(runtime_bitrate("bitrate", &Integer(1 << 40)), Some(Err(_))));
		assert!(matches!(runtime_bitrate("bitrate", &Double(5e5)), Some(Err(_))));
		assert_eq!(runtime_bitrate("fps", &Integer(10)), None);
	}

	#[test]
	fn restart_delays_back_off_and_reset() {
		let mut restarts = Restarts::default();
		let delays: Vec<u64> = (0..7).map(|_| restarts.next("topic /a", false).as_secs()).collect();
		assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30]);
		// Each bridge has its own delay.
		assert_eq!(restarts.next("topic /b", false), Restarts::MIN);
		// A bridge that got messages through starts over.
		assert_eq!(restarts.next("topic /a", true), Restarts::MIN);
		assert_eq!(restarts.next("topic /a", false), Duration::from_secs(2));
	}
}
