# moqros

Stream ROS 2 `sensor_msgs/Image` topics over [Media over QUIC](https://moq.dev) (MoQ), in Rust.

moqros converts each image to I420, encodes it to **VP8** with [libvpx](https://chromium.googlesource.com/webm/libvpx)
(royalty-free; the default), and publishes it to a MoQ relay as a [hang](https://docs.rs/hang) broadcast.
That's the WebCodecs-friendly format played by the official
[`@moq/watch`](https://www.npmjs.com/package/@moq/watch) web component. It can also go the other way:
subscribe to a broadcast, decode it, and republish it as a ROS topic. Optional **H.264** support
(`h264` feature) loads Cisco's prebuilt OpenH264 binary at runtime.

```
 ROS 2 topic ──► moqros-pub ──► moq-relay ──► browser (<moq-watch>)
 sensor_msgs/Image   VP8/hang       QUIC/WebTransport    └──► moqros-sub ──► ROS 2 topic
```

## Layout

| Path | What |
| --- | --- |
| `crates/moqros` | The library. The core (conversion, codec, MoQ publish/subscribe) is ROS-independent; the `ros` feature adds r2r bridges. |
| `crates/moqros-cli` | Bridge nodes: `moqros-pub` (ROS → MoQ) and `moqros-sub` (MoQ → ROS). |
| `examples/image_source` | Sample node that plays a video file (via ffmpeg) or a PNG/JPEG directory as an Image topic. |
| `examples/web` | Static browser viewer built on `@moq/watch`. |
| `docker/` | Dockerfile (ROS 2 Jazzy + Rust, moq-relay) and the end-to-end `compose.yml`. |

## End-to-end sample (Docker)

You need Docker and ffmpeg (ffmpeg only generates the sample inputs).

```sh
./scripts/gen_sample.sh                      # samples/sample.mp4 + samples/frames/*.png
docker compose -f docker/compose.yml up --build
```

Open <http://localhost:8080> in a Chromium-based browser or Firefox. Both support WebTransport. The page
connects to `http://localhost:4443/anon` and plays the broadcast `sample/image_raw`.

The compose file runs these services:

- `relay`: `moq-relay` with a self-signed certificate. It allows anonymous access under `anon/`.
- `source`: `image_source` publishes `/sample/image_raw` at 30 fps as `bgr8` (1280x720; set `WIDTH=640` to scale down).
- `moqros-pub`: subscribes to `/sample/image_raw` and publishes the MoQ broadcast `sample/image_raw`.
- `moqros-sub`: subscribes to that broadcast and republishes decoded `rgb8` images on `/moqros/image`.
- `web`: nginx serving `examples/web`.

To play the PNG sequence instead of the video, run `SAMPLE=frames docker compose -f docker/compose.yml up`.

The ROS services share one network and IPC namespace, as processes on one robot would. This lets Fast DDS
use shared memory for the 2.7 MB frames. The source publishes with reliable QoS, and `moqros-pub` runs with
`--reliable`. With best-effort QoS, large images are split into many UDP fragments, and losing any one of them
drops the whole frame. In testing that cut 30 fps down to 10–20 fps. On a real robot, match the camera
driver's QoS. Both bridges log their frame rate every 10 s (`image rate`).

To check the round trip on the ROS side:

```sh
docker compose -f docker/compose.yml exec moqros-sub bash -c \
  'source /opt/ros/jazzy/setup.bash && ROS_DOMAIN_ID=42 ros2 topic hz /moqros/image'
```

If UDP port forwarding doesn't work in your Docker setup, run the relay on the host instead
(`cargo install moq-relay && moq-relay docker/relay.toml`).

## Library usage

```toml
[dependencies]
moqros = { git = "…", features = ["ros"] }   # omit `ros` for the ROS-independent core
```

### ROS → MoQ

```rust
use moqros::ros::{QosProfile, r2r, spawn_ros_to_moq};

let ctx = r2r::Context::create()?;
let mut node = r2r::Node::create(ctx, "camera_streamer", "")?;

let publisher = moqros::Publisher::connect(&moqros::ClientConfig::new(
    "http://localhost:4443/anon".parse()?,
))?;
let broadcast = publisher.create_image_broadcast("camera", moqros::EncoderSettings::default())?;
let bridge = spawn_ros_to_moq(&mut node, "/camera/image_raw", QosProfile::sensor_data(), broadcast)?;

tokio::task::spawn_blocking(move || loop {
    node.spin_once(std::time::Duration::from_millis(50));
});
bridge.await??;
```

### Without ROS

```rust
let publisher = moqros::Publisher::connect(&config)?;
let mut camera = publisher.create_image_broadcast("camera", Default::default())?;
camera.publish(&moqros::Image::new(640, 480, moqros::PixelFormat::Rgb8, rgb_bytes, capture_time)?)?;

let subscriber = moqros::Subscriber::connect(&config)?;
let mut images = subscriber.subscribe_images("camera").await?;
while let Some(image) = images.next_image().await? {
    // image is rgb8
}
```

### Codecs

| Codec | Feature | Backend | Notes |
| --- | --- | --- | --- |
| VP8 | `vp8` (default) | system libvpx, real-time CBR | Royalty-free. |
| H.264 | `h264` | Cisco's OpenH264 binary, loaded at runtime | Constrained Baseline, `avc3`. Never compiled from source (see below). |

Pick the codec with `EncoderSettings::codec` (`moqros-pub --codec vp8|h264`). The subscriber picks the best
rendition in the catalog that this build can decode.

**H.264 and patents.** Cisco pays the H.264 patent royalties for OpenH264 only for **binaries downloaded from
Cisco**. A library built from source isn't covered. So the `h264` feature never compiles OpenH264. It loads
Cisco's binary at runtime instead and checks its SHA-256 against known Cisco releases:

```sh
export OPENH264_LIBRARY=$(./scripts/fetch_openh264.sh)   # downloads from ciscobinary.openh264.org
cargo run -p moqros-cli --features h264 --bin moqros-pub -- --codec h264 ...
```

You can also set the path in code with `moqros::set_openh264_library`.

### Behavior

- **Encodings:** `rgb8`, `bgr8`, `rgba8`, `bgra8`, `mono8`, `yuv422` (UYVY), and `yuv422_yuy2` (YUYV). An odd
  trailing row or column is cropped, because 4:2:0 video needs even dimensions.
- **Format on the wire:** each frame goes in the hang "legacy" container (a timestamp plus the codec payload). The catalog
  (`catalog.json`) has one video track named `video`. It's republished whenever the resolution changes.
- **Groups:** each keyframe starts a new MoQ group. The keyframe interval (default 2 s) bounds how long
  a new viewer waits for the first picture.
- **Latency:** if the encoder falls behind, the bridge drops all but the newest pending image. The subscriber
  always jumps to the newest group.
- **Timestamps:** `header.stamp` becomes a µs timeline that starts at 0 for each broadcast.
  `moqros-sub` stamps the republished images with the local receive time.
- **Reconnects:** the relay connection redials automatically. `moqros-sub` resubscribes when the broadcast
  comes back.
- **QoS:** both bridges default to the best-effort `sensor_data` profile. Pass `--reliable` to match a
  reliable publisher, which is recommended for large uncompressed images.

## Development

The core library builds and tests natively on any OS. ROS isn't required for it, but the default `vp8`
feature needs libvpx, pkg-config, and libclang (bindgen generates the libvpx bindings):

```sh
brew install libvpx pkgconf            # macOS (libclang comes with the Xcode CLI tools)
sudo apt install libvpx-dev pkg-config libclang-dev   # Debian/Ubuntu

cargo test -p moqros
# H.264 tests (skipped unless OPENH264_LIBRARY is set):
OPENH264_LIBRARY=$(./scripts/fetch_openh264.sh) cargo test -p moqros --features h264
# Integration test through a real relay:
cargo install moq-relay
MOQROS_RELAY_BIN=$(which moq-relay) cargo test -p moqros --test relay
```

The `ros` feature, `moqros-cli`, and `examples/image_source` need a sourced ROS 2 environment and libclang,
because r2r generates its bindings at build time. Use the Docker `build` stage, or on a ROS machine:

```sh
source /opt/ros/jazzy/setup.bash
IDL_PACKAGE_FILTER="std_msgs;sensor_msgs;geometry_msgs;rosgraph_msgs" cargo build --workspace
```

## License

MIT OR Apache-2.0

Dependencies have their own licenses. libvpx is BSD-3-Clause, and the `env-libvpx-sys` bindings are MPL-2.0.
Cisco's OpenH264 binary is covered by [Cisco's binary license](https://www.openh264.org/BINARY_LICENSE.txt).
