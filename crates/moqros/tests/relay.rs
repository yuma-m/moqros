//! End-to-end test through a real `moq-relay` process.
//!
//! Skipped unless `MOQROS_RELAY_BIN` points at a moq-relay binary:
//!
//! ```sh
//! cargo install moq-relay
//! MOQROS_RELAY_BIN=$(which moq-relay) cargo test -p moqros --test relay
//! ```

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use moqros::{
	ClientConfig, Codec, DataFormat, DataMode, DataSettings, EncoderSettings, Image, PixelFormat, Publisher,
	Subscriber, Subscription,
};

struct Relay(Child);

impl Drop for Relay {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

fn start_relay(bin: &str, port: u16) -> Relay {
	start_relay_with(
		bin,
		concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker/relay.toml"),
		port,
	)
}

fn start_relay_with(bin: &str, config: impl AsRef<Path>, port: u16) -> Relay {
	let child = Command::new(bin)
		.arg(config.as_ref())
		.env("MOQ_LISTEN", format!("127.0.0.1:{port}"))
		.env("MOQ_WEB_HTTP_LISTEN", format!("127.0.0.1:{port}"))
		.stdout(Stdio::null())
		.stderr(Stdio::null())
		.spawn()
		.expect("failed to spawn moq-relay");
	Relay(child)
}

#[expect(clippy::cast_possible_truncation, reason = "the pattern wraps around")]
fn frame(width: u32, height: u32, n: u32) -> Image {
	let data: Vec<u8> = (0..height)
		.flat_map(|y| (0..width).flat_map(move |x| [((x + n * 8) % 256) as u8, (y % 256) as u8, 200]))
		.collect();
	Image::new(
		width,
		height,
		PixelFormat::Rgb8,
		data,
		Duration::from_millis(u64::from(n) * 33),
	)
	.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_and_subscribe_through_relay() -> anyhow::Result<()> {
	let Ok(bin) = std::env::var("MOQROS_RELAY_BIN") else {
		eprintln!("MOQROS_RELAY_BIN not set; skipping relay test");
		return Ok(());
	};
	let _ = tracing_subscriber::fmt().with_env_filter("moqros=debug").try_init();

	let port = 14443;
	let _relay = start_relay(&bin, port);
	tokio::time::sleep(Duration::from_millis(500)).await;

	let config = ClientConfig::new(format!("http://127.0.0.1:{port}/anon").parse()?);

	let publisher = Publisher::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), publisher.established()).await??;
	let subscriber = Subscriber::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), subscriber.established()).await??;

	for &codec in Codec::available() {
		if codec == Codec::H264 && std::env::var_os("OPENH264_LIBRARY").is_none() {
			eprintln!("OPENH264_LIBRARY not set; skipping H.264 through the relay");
			continue;
		}
		images_through_relay(&publisher, &subscriber, codec).await?;
	}
	Ok(())
}

async fn images_through_relay(publisher: &Publisher, subscriber: &Subscriber, codec: Codec) -> anyhow::Result<()> {
	let name = format!("test/image-{codec}");
	let mut broadcast = publisher.create_image_broadcast(
		&name,
		EncoderSettings {
			codec,
			keyframe_interval: Duration::from_millis(200),
			..Default::default()
		},
	)?;

	// Publish continuously from a blocking thread, like a ROS callback would.
	let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
	let pub_task = tokio::task::spawn_blocking(move || -> moqros::Result<()> {
		for n in 0.. {
			if stop_rx.try_recv().is_ok() {
				break;
			}
			broadcast.publish(&frame(160, 120, n))?;
			std::thread::sleep(Duration::from_millis(33));
		}
		Ok(())
	});

	let mut images = tokio::time::timeout(Duration::from_secs(10), subscriber.subscribe_images(&name)).await??;
	assert_eq!(images.codec(), codec);

	let mut received = Vec::new();
	while received.len() < 10 {
		let image = tokio::time::timeout(Duration::from_secs(5), images.next_image())
			.await??
			.expect("broadcast ended early");
		assert_eq!(
			(image.width, image.height, image.format),
			(160, 120, PixelFormat::Rgb8),
			"{codec}"
		);
		received.push(image.timestamp);
	}
	assert!(
		received.windows(2).all(|w| w[0] < w[1]),
		"{codec}: timestamps increase: {received:?}"
	);

	stop_tx.send(()).ok();
	pub_task.await??;
	Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn publish_and_subscribe_data_through_relay() -> anyhow::Result<()> {
	let Ok(bin) = std::env::var("MOQROS_RELAY_BIN") else {
		eprintln!("MOQROS_RELAY_BIN not set; skipping relay test");
		return Ok(());
	};
	let _ = tracing_subscriber::fmt().with_env_filter("moqros=debug").try_init();

	let port = 14444;
	let _relay = start_relay(&bin, port);
	tokio::time::sleep(Duration::from_millis(500)).await;

	let config = ClientConfig::new(format!("http://127.0.0.1:{port}/anon").parse()?);
	let publisher = Publisher::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), publisher.established()).await??;
	let subscriber = Subscriber::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), subscriber.established()).await??;

	for (format, mode) in [
		(DataFormat::Json, DataMode::Snapshot),
		(DataFormat::Cdr, DataMode::Stream),
	] {
		let name = format!("test/{format}");
		let settings = DataSettings {
			format,
			mode,
			ros_type: Some("std_msgs/msg/String".into()),
		};
		let mut broadcast = publisher.create_data_broadcast(&name, settings.clone())?;

		// Publish continuously until stopped, so the subscriber can join at any point.
		let (stop_tx, mut stop_rx) = tokio::sync::oneshot::channel::<()>();
		let pub_task: tokio::task::JoinHandle<moqros::Result<()>> = tokio::spawn(async move {
			for n in 0u32.. {
				if stop_rx.try_recv().is_ok() {
					break;
				}
				broadcast.publish(n.to_string())?;
				tokio::time::sleep(Duration::from_millis(10)).await;
			}
			Ok(())
		});

		let subscription = tokio::time::timeout(Duration::from_secs(10), subscriber.subscribe(&name)).await??;
		let Subscription::Data(mut messages) = subscription else {
			panic!("{name}: expected a data subscription");
		};
		assert_eq!(messages.settings(), &settings);

		let mut received = Vec::new();
		while received.len() < 10 {
			let message = tokio::time::timeout(Duration::from_secs(5), messages.next_message())
				.await??
				.expect("broadcast ended early");
			received.push(std::str::from_utf8(&message.payload)?.parse::<u32>()?);
		}
		assert!(received.windows(2).all(|w| w[0] < w[1]), "{name}: {received:?}");
		if mode == DataMode::Stream {
			assert!(
				received.windows(2).all(|w| w[1] == w[0] + 1),
				"{name} is lossless: {received:?}"
			);
		}

		stop_tx.send(()).ok();
		pub_task.await??;
	}
	Ok(())
}

/// Read the next message of `name` on `subscriber` as a number, retrying until one arrives.
async fn first_number(subscriber: &Subscriber, name: &str) -> anyhow::Result<u32> {
	let Subscription::Data(mut messages) = subscriber.subscribe(name).await? else {
		panic!("{name}: expected a data subscription");
	};
	let message = messages.next_message().await?.expect("broadcast ended early");
	Ok(std::str::from_utf8(&message.payload)?.parse()?)
}

#[tokio::test(flavor = "multi_thread")]
async fn one_connection_publishes_and_subscribes() -> anyhow::Result<()> {
	let Ok(bin) = std::env::var("MOQROS_RELAY_BIN") else {
		eprintln!("MOQROS_RELAY_BIN not set; skipping relay test");
		return Ok(());
	};
	let _ = tracing_subscriber::fmt().with_env_filter("moqros=debug").try_init();

	let port = 14447;
	let _relay = start_relay(&bin, port);
	tokio::time::sleep(Duration::from_millis(500)).await;
	let config = ClientConfig::new(format!("http://127.0.0.1:{port}/anon").parse()?);

	// A robot that streams its state and takes commands, and an operator doing the opposite,
	// each over a single session.
	let (robot_pub, robot_sub) = moqros::connect_both(&config)?;
	let (operator_pub, operator_sub) = moqros::connect_both(&config)?;
	tokio::time::timeout(Duration::from_secs(10), robot_pub.established()).await??;
	tokio::time::timeout(Duration::from_secs(10), operator_sub.established()).await??;

	let settings = DataSettings::new(DataFormat::Json);
	let mut state = robot_pub.create_data_broadcast("robot/state", settings.clone())?;
	let mut command = operator_pub.create_data_broadcast("operator/cmd", settings)?;
	let publishing: tokio::task::JoinHandle<moqros::Result<()>> = tokio::spawn(async move {
		for n in 0u32.. {
			state.publish(n.to_string())?;
			command.publish((n + 1000).to_string())?;
			tokio::time::sleep(Duration::from_millis(20)).await;
		}
		Ok(())
	});

	let to_operator =
		tokio::time::timeout(Duration::from_secs(10), first_number(&operator_sub, "robot/state")).await??;
	let to_robot = tokio::time::timeout(Duration::from_secs(10), first_number(&robot_sub, "operator/cmd")).await??;
	assert!(to_operator < 1000, "robot state: {to_operator}");
	assert!(to_robot >= 1000, "operator command: {to_robot}");

	// Both handles close the one session.
	robot_sub.close().await?;
	tokio::time::timeout(Duration::from_secs(5), robot_pub.closed()).await??;
	publishing.abort();
	Ok(())
}

/// A CA, and a client certificate it signed, written as PEM files to `dir`.
fn write_client_certs(dir: &Path) -> anyhow::Result<()> {
	use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair};

	let ca_key = KeyPair::generate()?;
	let mut ca = CertificateParams::new(Vec::<String>::new())?;
	ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
	std::fs::write(dir.join("ca.pem"), ca.self_signed(&ca_key)?.pem())?;
	let issuer = Issuer::new(ca, ca_key);

	let key = KeyPair::generate()?;
	let mut client = CertificateParams::new(vec!["robot".to_string()])?;
	client.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
	std::fs::write(dir.join("robot.pem"), client.signed_by(&key, &issuer)?.pem())?;
	std::fs::write(dir.join("robot.key"), key.serialize_pem())?;
	Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn client_certificate_authenticates() -> anyhow::Result<()> {
	let Ok(bin) = std::env::var("MOQROS_RELAY_BIN") else {
		eprintln!("MOQROS_RELAY_BIN not set; skipping relay test");
		return Ok(());
	};
	let _ = tracing_subscriber::fmt().with_env_filter("moqros=debug").try_init();

	let dir = std::env::temp_dir().join(format!("moqros-mtls-{}", std::process::id()));
	std::fs::create_dir_all(&dir)?;
	write_client_certs(&dir)?;

	// The reference auth server grants certificates `robot/**` and nothing to anyone else.
	let mut policy = moq_auth::serve::Policy::default();
	let robot: moq_auth::Patterns = ["robot/**".parse()?].into_iter().collect();
	policy.mtls = moq_auth::Permissions::new(robot.clone(), robot);
	let auth = moq_auth::serve::Server::new(policy)?;
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
	let auth_url = format!("http://{}/", listener.local_addr()?);
	tokio::spawn(async move { auth.serve(listener).await });

	let port = 14445;
	let config = dir.join("relay.toml");
	std::fs::write(
		&config,
		format!(
			r#"[listen]
bind = "127.0.0.1:{port}"
tls.generate = ["localhost"]
tls.root = ["{ca}"]

[web.http]
listen = "127.0.0.1:{port}"

[auth]
url = "{auth_url}"
"#,
			ca = dir.join("ca.pem").display(),
		),
	)?;
	let _relay = start_relay_with(&bin, &config, port);
	tokio::time::sleep(Duration::from_millis(500)).await;

	let url: moqros::Url = format!("http://127.0.0.1:{port}/robot").parse()?;

	// Without a certificate the relay refuses the session.
	let anonymous = Publisher::connect(&ClientConfig::new(url.clone()))?;
	let refused = tokio::time::timeout(Duration::from_secs(10), anonymous.established()).await?;
	assert!(
		matches!(&refused, Err(moqros::Error::Connection(err)) if err.is_auth()),
		"{refused:?}"
	);

	let config = ClientConfig::new(url.clone()).with_client_cert(dir.join("robot.pem"), dir.join("robot.key"));
	let publisher = Publisher::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), publisher.established()).await??;
	let subscriber = Subscriber::connect(&config)?;
	tokio::time::timeout(Duration::from_secs(10), subscriber.established()).await??;

	let mut broadcast = publisher.create_data_broadcast("status", DataSettings::new(DataFormat::Json))?;
	let pub_task: tokio::task::JoinHandle<moqros::Result<()>> = tokio::spawn(async move {
		loop {
			broadcast.publish(r#"{"ok":true}"#)?;
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	});
	let mut messages = tokio::time::timeout(Duration::from_secs(10), subscriber.subscribe_data("status")).await??;
	let message = tokio::time::timeout(Duration::from_secs(5), messages.next_message())
		.await??
		.expect("broadcast ended early");
	assert_eq!(&message.payload[..], br#"{"ok":true}"#);

	// Without a certificate the relay refuses the session, so nothing arrives.
	let anonymous = Subscriber::connect(&ClientConfig::new(url))?;
	let refused = async {
		let mut messages = anonymous.subscribe_data("status").await?;
		messages.next_message().await
	};
	assert!(
		!matches!(
			tokio::time::timeout(Duration::from_secs(2), refused).await,
			Ok(Ok(Some(_)))
		),
		"a session without a certificate receives nothing"
	);

	pub_task.abort();
	std::fs::remove_dir_all(&dir).ok();
	Ok(())
}
