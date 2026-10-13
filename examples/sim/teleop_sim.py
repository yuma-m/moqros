#!/usr/bin/env python3
"""A tiny differential-drive robot in a 2D grid world, for the teleop demo.

One node, `teleop_sim`:

- subscribes to `/cmd_vel` (geometry_msgs/msg/Twist; `linear.x` and `angular.z`), and stops the
  robot when no command arrives for `cmd_timeout` seconds,
- publishes `/camera/image_raw`, a first-person raycast view (rgb8),
- publishes `/map/image_raw`, a top-down map with the robot (rgb8),
- publishes `/odom` (nav_msgs/msg/Odometry).

Only rclpy and numpy; no GPU or display needed.

    teleop_sim --ros-args -p camera_width:=640 -p camera_height:=360 -p camera_fps:=15.0
"""

import math
import time

import numpy as np
import rclpy
from geometry_msgs.msg import Twist
from nav_msgs.msg import Odometry
from rclpy.executors import ExternalShutdownException
from rclpy.node import Node
from rclpy.qos import QoSProfile, ReliabilityPolicy
from sensor_msgs.msg import Image

# '#' and letters are walls (the letter picks the color), '.' is free space, 'S' is the start.
WORLD = [
    "################",
    "#S.....#.......#",
    "#......#..rr...#",
    "#..gg..#..rr...#",
    "#..gg.........b#",
    "#.............b#",
    "#####..####....#",
    "#..............#",
    "#..y.......#...#",
    "#..y...bb..#...#",
    "#......bb..#...#",
    "#..........#####",
    "#....rr........#",
    "#....rr....yy..#",
    "#..............#",
    "################",
]

# RGB of each wall kind.
WALL_COLORS = {
    "#": (170, 170, 180),
    "r": (200, 70, 60),
    "g": (70, 170, 90),
    "b": (70, 110, 200),
    "y": (220, 190, 60),
}

MAX_LINEAR = 1.0  # m/s
MAX_ANGULAR = 1.5  # rad/s
RADIUS = 0.2  # m, for collisions
FOV = math.radians(70)
MAX_DEPTH = 24.0  # m
RAY_STEP = 0.02  # m


def parse_world(rows):
    """Wall kind index per cell (0 = free), the color table, and the start cell."""
    kinds = list(WALL_COLORS)
    grid = np.zeros((len(rows), len(rows[0])), dtype=np.uint8)
    start = (1.5, 1.5)
    for y, row in enumerate(rows):
        for x, cell in enumerate(row):
            if cell in WALL_COLORS:
                grid[y, x] = kinds.index(cell) + 1
            elif cell == "S":
                start = (x + 0.5, y + 0.5)
    colors = np.array([(0, 0, 0)] + [WALL_COLORS[k] for k in kinds], dtype=np.float32)
    return grid, colors, start


def yaw_to_quaternion(yaw):
    return 0.0, 0.0, math.sin(yaw / 2), math.cos(yaw / 2)


class TeleopSim(Node):
    def __init__(self):
        super().__init__("teleop_sim")
        self.width = self.declare_parameter("camera_width", 640).value
        self.height = self.declare_parameter("camera_height", 360).value
        camera_fps = self.declare_parameter("camera_fps", 15.0).value
        self.map_size = self.declare_parameter("map_size", 320).value
        self.cmd_timeout = self.declare_parameter("cmd_timeout", 0.5).value

        self.grid, self.colors, (self.x, self.y) = parse_world(WORLD)
        self.yaw = 0.0
        self.cmd = (0.0, 0.0)
        self.velocity = (0.0, 0.0)
        self.last_cmd = 0.0
        self.trail = []
        self.precompute_camera()

        # Reliable, like the cameras of the other sample: large best-effort images over
        # UDP lose many frames.
        images = QoSProfile(depth=5, reliability=ReliabilityPolicy.RELIABLE)
        self.camera_pub = self.create_publisher(Image, "/camera/image_raw", images)
        self.map_pub = self.create_publisher(Image, "/map/image_raw", images)
        self.odom_pub = self.create_publisher(Odometry, "/odom", 10)
        self.create_subscription(Twist, "/cmd_vel", self.on_cmd_vel, 10)

        self.dt = 0.02
        self.create_timer(self.dt, self.step)
        self.create_timer(1.0 / camera_fps, self.publish_camera)
        self.create_timer(0.2, self.publish_map)
        self.create_timer(0.1, self.publish_odom)
        self.get_logger().info(
            f"robot at ({self.x:.1f}, {self.y:.1f}); camera {self.width}x{self.height} at {camera_fps} fps"
        )

    def on_cmd_vel(self, msg):
        linear = float(np.clip(msg.linear.x, -MAX_LINEAR, MAX_LINEAR))
        angular = float(np.clip(msg.angular.z, -MAX_ANGULAR, MAX_ANGULAR))
        self.cmd = (linear, angular)
        self.last_cmd = time.monotonic()

    # ------------------------------------------------------------------ physics

    def blocked(self, x, y):
        """Whether a robot at (x, y) overlaps a wall cell."""
        for dx in (-RADIUS, RADIUS):
            for dy in (-RADIUS, RADIUS):
                cx, cy = int(x + dx), int(y + dy)
                if not (0 <= cy < self.grid.shape[0] and 0 <= cx < self.grid.shape[1]) or self.grid[cy, cx]:
                    return True
        return False

    def step(self):
        # Stop when the operator goes quiet: a dropped connection mustn't leave the robot driving.
        if time.monotonic() - self.last_cmd > self.cmd_timeout:
            self.cmd = (0.0, 0.0)
        linear, angular = self.cmd
        self.yaw = (self.yaw + angular * self.dt + math.pi) % (2 * math.pi) - math.pi
        # The map's y axis points down (rows), so positive yaw turns left as seen from above.
        nx = self.x + linear * math.cos(self.yaw) * self.dt
        ny = self.y - linear * math.sin(self.yaw) * self.dt
        # Slide along walls: try both axes, then each alone.
        moved = (nx, ny) if not self.blocked(nx, ny) else None
        if moved is None and not self.blocked(nx, self.y):
            moved = (nx, self.y)
        if moved is None and not self.blocked(self.x, ny):
            moved = (self.x, ny)
        if moved is not None:
            speed = math.hypot(moved[0] - self.x, moved[1] - self.y) / self.dt
            self.x, self.y = moved
            self.velocity = (math.copysign(speed, linear), angular)
        else:
            self.velocity = (0.0, angular)
        if not self.trail or math.hypot(self.trail[-1][0] - self.x, self.trail[-1][1] - self.y) > 0.1:
            self.trail = (self.trail + [(self.x, self.y)])[-300:]

    # ------------------------------------------------------------------ camera

    def precompute_camera(self):
        w, h = self.width, self.height
        # Ray angle offset of each column, and the distance of each sample along a ray.
        self.offsets = np.arctan(np.linspace(1, -1, w) * math.tan(FOV / 2)).astype(np.float32)
        self.depths = np.arange(RAY_STEP, MAX_DEPTH, RAY_STEP, dtype=np.float32)
        self.rows = np.arange(h, dtype=np.float32)[:, None]
        # Focal length in pixels, and the distance to the floor seen by each row below the horizon.
        self.focal = (w / 2) / math.tan(FOV / 2)
        below = np.arange(h // 2 + 1, h, dtype=np.float32) - h / 2
        self.floor_dist = (0.5 * self.focal / below)[:, None]
        sky = np.linspace(0, 1, h // 2 + 1, dtype=np.float32)[:, None, None]
        self.sky = (np.array([90, 150, 230]) * (1 - sky) + np.array([200, 225, 250]) * sky).astype(np.float32)

    def render_camera(self):
        w, h = self.width, self.height
        angles = self.yaw + self.offsets
        cos, sin = np.cos(angles), -np.sin(angles)
        # March every ray at once; the first sample inside a wall is the hit.
        xs = self.x + self.depths[:, None] * cos[None, :]
        ys = self.y + self.depths[:, None] * sin[None, :]
        cx = np.clip(xs.astype(np.int32), 0, self.grid.shape[1] - 1)
        cy = np.clip(ys.astype(np.int32), 0, self.grid.shape[0] - 1)
        cells = self.grid[cy, cx]
        hit = cells > 0
        index = np.where(hit.any(axis=0), hit.argmax(axis=0), len(self.depths) - 1)
        columns = np.arange(w)
        kind = cells[index, columns]
        dist = self.depths[index] * np.cos(self.offsets)  # perpendicular, so walls don't bend

        # Shade x-facing and y-facing walls differently, and fade with distance.
        hx, hy = xs[index, columns], ys[index, columns]
        fx, fy = np.abs(hx - np.round(hx)), np.abs(hy - np.round(hy))
        side = np.where(fx < fy, 1.0, 0.75)
        fade = np.clip(1.2 - dist / 14, 0.25, 1.0)
        wall = self.colors[kind] * (side * fade)[:, None]

        image = np.empty((h, w, 3), dtype=np.float32)
        image[: h // 2 + 1] = self.sky
        # Floor: a 1 m checkerboard, so motion shows even far from walls.
        fd = self.floor_dist / np.cos(self.offsets)[None, :]
        fx = self.x + fd * cos[None, :]
        fy = self.y + fd * sin[None, :]
        checker = (np.floor(fx).astype(np.int32) + np.floor(fy).astype(np.int32)) & 1
        floor_fade = np.clip(1.1 - fd / 16, 0.3, 1.0)[..., None]
        floor = np.where(checker[..., None] == 1, (110, 105, 95), (150, 145, 130)).astype(np.float32)
        image[h // 2 + 1 :] = floor * floor_fade

        half = (self.focal / np.maximum(dist, 1e-3)) / 2
        top = h / 2 - half
        bottom = h / 2 + half
        mask = (self.rows >= top[None, :]) & (self.rows < bottom[None, :])
        image = np.where(mask[..., None], wall[None, :, :], image)
        return image.astype(np.uint8)

    # ------------------------------------------------------------------ map

    def render_map(self):
        size = self.map_size
        rows, cols = self.grid.shape
        scale = size / max(rows, cols)
        py, px = np.mgrid[0:size, 0:size].astype(np.float32) / scale
        cells = self.grid[np.clip(py.astype(np.int32), 0, rows - 1), np.clip(px.astype(np.int32), 0, cols - 1)]
        image = np.where(cells[..., None] > 0, self.colors[cells], np.float32(245))
        # Faint 1 m grid.
        line = (np.abs(px - np.round(px)) < 0.5 / scale) | (np.abs(py - np.round(py)) < 0.5 / scale)
        image = np.where((line & (cells == 0))[..., None], np.float32(225), image)
        if self.trail:
            self.dots(image, np.array(self.trail), scale, 1, (120, 170, 240))
        robot = (px - self.x) ** 2 + (py - self.y) ** 2 < RADIUS**2
        image[robot] = (220, 40, 40)
        # Heading: a short line ahead of the robot.
        t = np.linspace(RADIUS, RADIUS * 2.5, 12)
        ahead = np.stack([self.x + t * math.cos(self.yaw), self.y - t * math.sin(self.yaw)], axis=1)
        self.dots(image, ahead, scale, 1, (40, 40, 40))
        return image.astype(np.uint8)

    @staticmethod
    def dots(image, points, scale, radius, color):
        """Paint a small square around each world point (x, y)."""
        size = image.shape[0]
        centers = np.round(points * scale).astype(np.int32)
        for dy in range(-radius, radius + 1):
            for dx in range(-radius, radius + 1):
                xs = np.clip(centers[:, 0] + dx, 0, size - 1)
                ys = np.clip(centers[:, 1] + dy, 0, size - 1)
                image[ys, xs] = color

    # ------------------------------------------------------------------ publishing

    def image_msg(self, pixels, frame_id):
        msg = Image()
        msg.header.stamp = self.get_clock().now().to_msg()
        msg.header.frame_id = frame_id
        msg.height, msg.width = pixels.shape[:2]
        msg.encoding = "rgb8"
        msg.step = msg.width * 3
        msg.data = pixels.tobytes()
        return msg

    def publish_camera(self):
        self.camera_pub.publish(self.image_msg(self.render_camera(), "camera"))

    def publish_map(self):
        self.map_pub.publish(self.image_msg(self.render_map(), "map"))

    def publish_odom(self):
        msg = Odometry()
        msg.header.stamp = self.get_clock().now().to_msg()
        msg.header.frame_id = "odom"
        msg.child_frame_id = "base_link"
        # ROS convention: x forward, y left; flip the map's downward rows.
        msg.pose.pose.position.x = self.x
        msg.pose.pose.position.y = -self.y
        q = yaw_to_quaternion(self.yaw)
        (
            msg.pose.pose.orientation.x,
            msg.pose.pose.orientation.y,
            msg.pose.pose.orientation.z,
            msg.pose.pose.orientation.w,
        ) = q
        msg.twist.twist.linear.x, msg.twist.twist.angular.z = self.velocity
        self.odom_pub.publish(msg)


def main():
    rclpy.init()
    node = TeleopSim()
    try:
        rclpy.spin(node)
    except (KeyboardInterrupt, ExternalShutdownException):
        pass
    finally:
        node.destroy_node()
        rclpy.try_shutdown()


if __name__ == "__main__":
    main()
