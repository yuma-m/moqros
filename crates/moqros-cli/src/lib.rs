#![doc = include_str!("../README.md")]
//!
//! ## Library
//!
//! The library part of this crate is shared plumbing for the bridge binaries, not a stable API.
//! Use [`moqros`] instead.

mod args;
mod bridges;
mod diagnostics;
mod params;
mod runtime;
mod spec;
mod url;

pub use args::{DiagnosticsArgs, EncoderArgs, Format, QosArgs, RelayArgs};
pub use bridges::{Bridges, Relay, run_bridges};
pub use params::{init, strip_ros_args};
pub use runtime::{SharedNode, Spinner, init_logging, shutdown_signal, spawn_spinner};
pub use spec::{
	BroadcastSpec, DEFAULT_SUB_TOPIC, DEFAULT_TOPIC_TEMPLATE, Discovery, PubTopic, SubBroadcast, TopicSpec,
	check_no_loop, parse_encoding, pub_topics, sub_broadcasts,
};
