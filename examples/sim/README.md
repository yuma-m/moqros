# teleop_sim

A tiny differential-drive robot in a 2D grid world, for the [teleop demo](../../docker/README.md#teleop-demo).
It only needs rclpy and numpy; no GPU or display.

- Subscribes to `/cmd_vel` (`geometry_msgs/msg/Twist`; `linear.x` and `angular.z`), and stops the robot when no
  command arrives for `cmd_timeout` seconds.
- Publishes `/camera/image_raw`, a first-person raycast view (`rgb8`).
- Publishes `/map/image_raw`, a top-down map with the robot (`rgb8`).
- Publishes `/odom` (`nav_msgs/msg/Odometry`).

```sh
source /opt/ros/jazzy/setup.bash
python3 examples/sim/teleop_sim.py --ros-args -p camera_width:=640 -p camera_height:=360 -p camera_fps:=15.0
```

| Parameter | Default | What |
| --- | --- | --- |
| `camera_width`, `camera_height` | `640`, `360` | Size of the camera image. |
| `camera_fps` | `15.0` | Publish rate of the camera. The map goes out at 5 Hz, the odometry at 10 Hz. |
| `map_size` | `320` | Width and height of the map image. |
| `cmd_timeout` | `0.5` | Seconds without a command before the robot stops. |
