# image_source

A sample ROS 2 node that plays a video file (decoded by ffmpeg) or a directory of PNG/JPEG frames as a
`sensor_msgs/msg/Image` topic at a fixed frame rate, looping forever. The [Docker sample](../../docker/README.md)
uses it as its camera.

It needs a sourced ROS 2 environment to build (see [Development](../../README.md#development)), and `ffmpeg`
and `ffprobe` on the `PATH` for video files.

```sh
cargo run -p moqros-image-source -- samples/sample.mp4 --topic /camera/image_raw --fps 30 --width 1280
cargo run -p moqros-image-source -- samples/frames --topic /camera/image_raw --once
```

| Flag | Default | What |
| --- | --- | --- |
| `--topic` | `/image_raw` | Topic to publish on. |
| `--fps` | `30` | Frames per second. |
| `--encoding` | `bgr8` | Pixel encoding of the published messages: `rgb8` or `bgr8`. |
| `--width` | | Scale video input to this width, keeping the aspect ratio. Ignored for image directories. |
| `--frame-id` | `camera` | `header.frame_id` of the published images. |
| `--once` | | Stop after one pass instead of looping. |
