//! Shared plumbing for the moqros bridge binaries.

use std::any::TypeId;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Context, bail};
use clap::builder::BoolishValueParser;
use clap::error::ErrorKind;
use clap::{ArgAction, Args, Command, Parser};
use moqros::ros::r2r::ParameterValue;
use moqros::ros::{QosProfile, r2r};
use tokio::task::JoinHandle;

/// Relay connection flags.
#[derive(Debug, Clone, Args)]
pub struct RelayArgs {
	/// Relay URL; its path scopes broadcast names.
	#[arg(long, env = "MOQROS_URL", default_value = "http://localhost:4443/anon")]
	pub url: moqros::Url,

	/// Disable TLS certificate verification (development only).
	#[arg(long, env = "MOQROS_INSECURE", action = ArgAction::Set, value_parser = BoolishValueParser::new(),
		num_args = 0..=1, require_equals = true, default_value_t = false, default_missing_value = "true")]
	pub insecure: bool,
}

impl RelayArgs {
	pub fn config(&self) -> moqros::ClientConfig {
		moqros::ClientConfig {
			url: self.url.clone(),
			insecure: self.insecure,
		}
	}
}

/// ROS QoS flags.
#[derive(Debug, Clone, Args)]
pub struct QosArgs {
	/// Use reliable QoS instead of the best-effort sensor-data profile.
	#[arg(long, action = ArgAction::Set, value_parser = BoolishValueParser::new(),
		num_args = 0..=1, require_equals = true, default_value_t = false, default_missing_value = "true")]
	pub reliable: bool,
}

impl QosArgs {
	pub fn profile(&self) -> QosProfile {
		if self.reliable {
			QosProfile::default().keep_last(5)
		} else {
			QosProfile::sensor_data()
		}
	}
}

/// Create the ROS node and parse `T` from the command line and the node's ROS parameters.
///
/// Arguments between `--ros-args` and `--` are left to rcl, so the binaries accept
/// remapping (`-r __node:=…`, `-r __ns:=…`), `-p` and `--params-file` like any ROS
/// node. Each ROS parameter named after an argument (`bitrate`, `keyframe_interval_ms`,
/// …) fills that argument: flags win over parameters, which win over environment
/// variables and defaults.
pub fn init<T: Parser>(node_name: &str) -> anyhow::Result<(T, r2r::Node)> {
	let args = strip_ros_args(std::env::args());
	// Answer `--help` and `--version` without bringing up ROS.
	if let Some(err) = help_or_version(T::command(), &args) {
		err.exit();
	}

	let ctx = r2r::Context::create().context("failed to create ROS context")?;
	let node = r2r::Node::create(ctx, node_name, "").context("failed to create ROS node")?;

	let flags = param_flags(
		&T::command(),
		node.params
			.lock()
			.unwrap()
			.iter()
			.map(|(name, param)| (name.as_str(), &param.value)),
	)?;
	let cli = match parse_args(args, flags) {
		Ok(cli) => cli,
		Err(ParseError::CommandLine(err)) => err.exit(),
		Err(ParseError::Param(err)) => {
			// Just clap's first line; its usage hints are about the command line.
			let message = err.to_string();
			let message = message.lines().next().unwrap_or_default().trim_start_matches("error: ");
			bail!("invalid ROS parameter: {message}");
		}
	};
	Ok((cli, node))
}

/// Drop the ROS arguments (`--ros-args … [--]`, possibly repeated) from `args`.
pub fn strip_ros_args(args: impl IntoIterator<Item = String>) -> Vec<String> {
	let mut in_ros_args = false;
	args.into_iter()
		.filter(|arg| match (in_ros_args, arg.as_str()) {
			(false, "--ros-args") | (true, "--") => {
				in_ros_args = !in_ros_args;
				false
			}
			_ => !in_ros_args,
		})
		.collect()
}

/// The error to exit with if `args` ask for help or the version.
///
/// Clap answers these as soon as it meets the flag, before checking anything else.
fn help_or_version(command: Command, args: &[String]) -> Option<clap::Error> {
	let err = command.try_get_matches_from(args).err()?;
	matches!(err.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion).then_some(err)
}

/// Why [`parse_args`] failed.
#[derive(Debug)]
enum ParseError {
	/// The command line itself is invalid.
	CommandLine(clap::Error),
	/// The command line is fine on its own, so a ROS parameter is invalid.
	Param(clap::Error),
}

/// Parse `T` from `args` (program name first, ROS arguments already stripped) and
/// `param_flags`.
///
/// The parameter flags go right after the program name. With `args_override_self` a
/// later occurrence of a flag wins, so the command line overrides them, while
/// environment variables and defaults only apply to flags given neither way.
fn parse_args<T: Parser>(args: Vec<String>, param_flags: Vec<String>) -> Result<T, ParseError> {
	let parse = |param_flags: Vec<String>| {
		let mut args = args.iter().cloned();
		let args = args.next().into_iter().chain(param_flags).chain(args);
		let matches = T::command().args_override_self(true).try_get_matches_from(args)?;
		T::from_arg_matches(&matches)
	};
	parse(param_flags).map_err(|err| match parse(Vec::new()) {
		Ok(_) => ParseError::Param(err),
		Err(err) => ParseError::CommandLine(err),
	})
}

/// Command-line flags (`--name=value`) for the ROS parameters named after an argument
/// of `command`.
fn param_flags<'a>(
	command: &Command,
	params: impl IntoIterator<Item = (&'a str, &'a ParameterValue)>,
) -> anyhow::Result<Vec<String>> {
	let mut flags = Vec::new();
	for (name, value) in params {
		let Some((long, arg)) = command
			.get_arguments()
			.find(|arg| arg.get_id() == name)
			.and_then(|arg| Some((arg.get_long()?, arg)))
		else {
			// r2r reads `use_sim_time` itself.
			if name != "use_sim_time" {
				tracing::warn!(name, "ignoring unknown ROS parameter");
			}
			continue;
		};
		let value = match value {
			// An empty YAML value: leave the flag to its environment variable or default.
			ParameterValue::NotSet => continue,
			ParameterValue::Bool(value) => value.to_string(),
			ParameterValue::Integer(value) => value.to_string(),
			// Keep the fraction of whole numbers for strings (`1.0` stays `1.0`), but drop
			// it for numbers so `2000000.0` still fits an integer flag.
			ParameterValue::Double(value) if arg.get_value_parser().type_id() == TypeId::of::<String>() => {
				format!("{value:?}")
			}
			ParameterValue::Double(value) => value.to_string(),
			ParameterValue::String(value) => value.clone(),
			value => bail!("ROS parameter `{name}` has unsupported type: {value:?}"),
		};
		// One token, so values starting with `-` aren't taken for flags.
		flags.push(format!("--{long}={value}"));
	}
	Ok(flags)
}

pub fn init_logging() {
	tracing_subscriber::fmt()
		.with_env_filter(
			tracing_subscriber::EnvFilter::try_from_default_env()
				.unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
		)
		.init();
}

/// Spin `node` on a blocking thread until the returned guard is dropped.
///
/// The tokio runtime waits for blocking tasks on shutdown, so a spinner that never
/// stops would keep the process alive after `main` returns.
pub fn spawn_spinner(mut node: r2r::Node) -> Spinner {
	let stop = Arc::new(AtomicBool::new(false));
	let stopped = stop.clone();
	let task = tokio::task::spawn_blocking(move || {
		while !stopped.load(Ordering::Relaxed) {
			node.spin_once(Duration::from_millis(50));
		}
	});
	Spinner { stop, task: Some(task) }
}

/// Stops the [`spawn_spinner`] thread when dropped.
pub struct Spinner {
	stop: Arc<AtomicBool>,
	task: Option<JoinHandle<()>>,
}

impl Spinner {
	/// Resolves if the spinner thread dies, which only happens when it panics, since
	/// without spinning no ROS message arrives anymore. Never resolves again after that.
	pub async fn failed(&mut self) -> anyhow::Error {
		let Some(task) = self.task.as_mut() else {
			return std::future::pending().await;
		};
		let result = task.await;
		self.task = None;
		match result {
			Err(err) => anyhow::Error::new(err).context("ROS spinner failed"),
			Ok(()) => anyhow::anyhow!("ROS spinner stopped"),
		}
	}
}

impl Drop for Spinner {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
	}
}

#[cfg(test)]
mod tests {
	use clap::CommandFactory;

	use super::*;

	fn strip(args: &[&str]) -> Vec<String> {
		strip_ros_args(args.iter().map(|arg| arg.to_string()))
	}

	#[derive(Debug, Parser)]
	#[command(version = "1.2.3")]
	struct TestCli {
		#[arg(long, default_value = "/image_raw")]
		topic: String,

		#[arg(long)]
		broadcast: Option<String>,

		#[arg(long, default_value_t = 30.0)]
		fps: f32,

		#[arg(long, default_value_t = 2000)]
		keyframe_interval_ms: u64,

		#[command(flatten)]
		relay: RelayArgs,

		#[command(flatten)]
		qos: QosArgs,
	}

	/// Only parsed by `params_override_env`, in a child process of its own.
	#[derive(Debug, Parser)]
	struct EnvCli {
		#[arg(long, env = "MOQROS_TEST_BITRATE", default_value_t = 1)]
		bitrate: u32,
	}

	fn flags<T: Parser>(params: &[(&str, ParameterValue)]) -> anyhow::Result<Vec<String>> {
		param_flags(&T::command(), params.iter().map(|(name, value)| (*name, value)))
	}

	fn args(args: &[&str]) -> Vec<String> {
		std::iter::once("bin")
			.chain(args.iter().copied())
			.map(String::from)
			.collect()
	}

	fn parse<T: Parser>(args_: &[&str], params: &[(&str, ParameterValue)]) -> Result<T, ParseError> {
		parse_args(args(args_), flags::<T>(params).unwrap())
	}

	fn string(value: &str) -> ParameterValue {
		ParameterValue::String(value.to_string())
	}

	#[test]
	fn strips_ros_args() {
		assert_eq!(strip(&["bin", "--topic", "a"]), ["bin", "--topic", "a"]);
		assert_eq!(
			strip(&[
				"bin",
				"--topic",
				"a",
				"--ros-args",
				"-r",
				"__node:=n",
				"-p",
				"bitrate:=1"
			]),
			["bin", "--topic", "a"]
		);
		assert_eq!(
			strip(&[
				"bin",
				"--ros-args",
				"-r",
				"__ns:=/x",
				"--",
				"--reliable",
				"--ros-args",
				"-p",
				"a:=1"
			]),
			["bin", "--reliable"]
		);
	}

	#[test]
	fn params_fill_flags() {
		let cli: TestCli = parse(
			&[],
			&[
				("topic", string("/front/image_raw")),
				("keyframe_interval_ms", ParameterValue::Integer(500)),
				("reliable", ParameterValue::Bool(true)),
				("url", string("http://relay:4443/anon")),
			],
		)
		.unwrap();
		assert_eq!(cli.topic, "/front/image_raw");
		assert_eq!(cli.keyframe_interval_ms, 500);
		assert!(cli.qos.reliable);
		assert_eq!(cli.relay.url.as_str(), "http://relay:4443/anon");
		assert!(!cli.relay.insecure);
		assert_eq!(cli.broadcast, None);
	}

	#[test]
	fn command_line_overrides_params() {
		let params = [
			("topic", string("/param")),
			("reliable", ParameterValue::Bool(true)),
			("insecure", ParameterValue::Bool(false)),
		];
		let cli: TestCli = parse(&["--topic", "/flag", "--reliable=false", "--insecure"], &params).unwrap();
		assert_eq!(cli.topic, "/flag");
		assert!(!cli.qos.reliable);
		assert!(cli.relay.insecure);
	}

	/// Setting a variable while other tests read the environment is undefined behavior,
	/// so this test reruns itself in a child process for each value of the variable.
	#[test]
	fn params_override_env() {
		const VAR: &str = "MOQROS_TEST_BITRATE";
		let bitrate = ("bitrate", ParameterValue::Integer(2));
		match std::env::var(VAR).ok().as_deref() {
			None => {
				for value in ["bogus", "4"] {
					let output = std::process::Command::new(std::env::current_exe().unwrap())
						.args(["--exact", "tests::params_override_env"])
						.env(VAR, value)
						.output()
						.unwrap();
					let stdout = String::from_utf8_lossy(&output.stdout);
					assert!(
						output.status.success() && stdout.contains("1 passed"),
						"{VAR}={value}:\n{stdout}"
					);
				}
			}
			// An invalid variable is never looked at when a parameter or flag is set.
			Some("bogus") => {
				assert_eq!(parse::<EnvCli>(&[], std::slice::from_ref(&bitrate)).unwrap().bitrate, 2);
				assert_eq!(parse::<EnvCli>(&["--bitrate", "3"], &[]).unwrap().bitrate, 3);
				assert!(matches!(parse::<EnvCli>(&[], &[]), Err(ParseError::CommandLine(_))));
				let help = help_or_version(EnvCli::command(), &args(&["--help"]));
				assert_eq!(help.map(|err| err.kind()), Some(ErrorKind::DisplayHelp));
			}
			Some(value) => {
				assert_eq!(value, "4");
				assert_eq!(parse::<EnvCli>(&[], &[]).unwrap().bitrate, 4);
				assert_eq!(parse::<EnvCli>(&[], std::slice::from_ref(&bitrate)).unwrap().bitrate, 2);
				assert_eq!(parse::<EnvCli>(&["--bitrate=3"], &[bitrate]).unwrap().bitrate, 3);
			}
		}
	}

	#[test]
	fn doubles_keep_their_fraction() {
		let cli: TestCli = parse(
			&[],
			&[
				("broadcast", ParameterValue::Double(1.0)),
				("fps", ParameterValue::Double(12.5)),
			],
		)
		.unwrap();
		assert_eq!(cli.broadcast.as_deref(), Some("1.0"));
		assert_eq!(cli.fps, 12.5);

		// Whole numbers still fit integer flags.
		let cli: TestCli = parse(&[], &[("keyframe_interval_ms", ParameterValue::Double(500.0))]).unwrap();
		assert_eq!(cli.keyframe_interval_ms, 500);

		// YAML `fps: 15` is an integer parameter.
		let cli: TestCli = parse(&[], &[("fps", ParameterValue::Integer(15))]).unwrap();
		assert_eq!(cli.fps, 15.0);
	}

	#[test]
	fn values_starting_with_a_dash_stay_values() {
		let cli: TestCli = parse(&[], &[("broadcast", string("-x"))]).unwrap();
		assert_eq!(cli.broadcast.as_deref(), Some("-x"));
	}

	#[test]
	fn unset_and_unknown_params_are_ignored() {
		let params = [
			("broadcast", ParameterValue::NotSet),
			("use_sim_time", ParameterValue::Bool(false)),
			("bitrat", ParameterValue::Integer(1)),
			("keyframe-interval-ms", ParameterValue::Integer(1)),
			// Clap's own arguments aren't parameters.
			("help", ParameterValue::Bool(true)),
			("version", string("2")),
		];
		assert!(flags::<TestCli>(&params).unwrap().is_empty());
	}

	#[test]
	fn arrays_are_rejected() {
		let params = [("topic", ParameterValue::StringArray(vec!["/a".into()]))];
		let err = flags::<TestCli>(&params).unwrap_err();
		assert!(err.to_string().contains("`topic`"), "{err}");
	}

	#[test]
	fn invalid_param_values_are_blamed_on_the_param() {
		for (name, value) in [("keyframe_interval_ms", "soon"), ("url", "not a url")] {
			match parse::<TestCli>(&[], &[(name, string(value))]) {
				Err(ParseError::Param(err)) => assert_eq!(err.kind(), ErrorKind::ValueValidation, "{name}"),
				other => panic!("{name}: {other:?}"),
			}
		}
	}

	#[test]
	fn invalid_command_lines_are_blamed_on_the_command_line() {
		let params = [("keyframe_interval_ms", string("soon"))];
		for args in [&["--fps=fast"][..], &["--unknown"]] {
			assert!(
				matches!(parse::<TestCli>(args, &params), Err(ParseError::CommandLine(_))),
				"{args:?}"
			);
		}
	}

	#[test]
	fn detects_help_and_version() {
		let kind = |args_: &[&str]| help_or_version(TestCli::command(), &args(args_)).map(|err| err.kind());
		assert_eq!(kind(&["--help"]), Some(ErrorKind::DisplayHelp));
		assert_eq!(kind(&["-V"]), Some(ErrorKind::DisplayVersion));
		// Arguments after the help flag aren't looked at.
		assert_eq!(kind(&["-h", "--fps=fast"]), Some(ErrorKind::DisplayHelp));
		assert_eq!(kind(&["--topic", "/a"]), None);
		assert_eq!(kind(&["--fps=fast"]), None);
	}
}
