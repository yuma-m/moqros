#!/usr/bin/env python3
"""End-to-end checks for the Docker demos, run by docker/e2e/*.compose.yml.

    check.py sample   # docker/compose.yml: the four broadcasts come back on /moqros/*
    check.py teleop   # docker/teleop.compose.yml: video and /odom out, /cmd_vel in

Each check waits for messages on the topics the demo's bridges republish, validates them, and
exits non-zero when one fails or doesn't arrive before the deadline.
"""

import math
import sys
import time

import rclpy
from geometry_msgs.msg import PoseStamped, Twist
from nav_msgs.msg import Odometry
from rclpy.node import Node
from rclpy.qos import qos_profile_sensor_data
from sensor_msgs.msg import Image
from std_msgs.msg import Header

DEADLINE_S = 120.0


def image(width, height, frame_id=None):
    def check(msg):
        if (msg.width, msg.height, msg.encoding) != (width, height, "rgb8"):
            return f"got {msg.width}x{msg.height} {msg.encoding}, want {width}x{height} rgb8"
        if msg.step != width * 3 or len(msg.data) != msg.step * height:
            return f"step {msg.step} and {len(msg.data)} bytes don't match {width}x{height} rgb8"
        if frame_id is not None and msg.header.frame_id != frame_id:
            return f"frame_id {msg.header.frame_id!r}, want {frame_id!r}"
        return None

    return check


def header(frame_id):
    def check(msg):
        if msg.frame_id != frame_id:
            return f"frame_id {msg.frame_id!r}, want {frame_id!r}"
        if msg.stamp.sec == 0:
            return "stamp is zero"
        return None

    return check


def pose(msg):
    if msg.header.frame_id != "map":
        return f"frame_id {msg.header.frame_id!r}, want 'map'"
    p = msg.pose.position
    if (p.x, p.y) != (1.0, 0.5):
        return f"position ({p.x}, {p.y}), want (1.0, 0.5)"
    return None


def yaw(msg):
    q = msg.pose.pose.orientation
    return math.atan2(2 * (q.w * q.z + q.x * q.y), 1 - 2 * (q.y * q.y + q.z * q.z))


class Checker(Node):
    def __init__(self):
        super().__init__("moqros_e2e_check")
        self.pending = {}
        self.failures = []

    def expect(self, topic, msg_type, check):
        """Passes when the first message on `topic` passes `check` (None means it did)."""
        self.pending[topic] = True

        def on_msg(msg):
            if topic not in self.pending:
                return
            del self.pending[topic]
            error = check(msg)
            if error:
                self.failures.append(f"{topic}: {error}")
                self.get_logger().error(f"FAIL {topic}: {error}")
            else:
                self.get_logger().info(f"ok   {topic}")

        self.create_subscription(msg_type, topic, on_msg, qos_profile_sensor_data)

    def expect_turn(self, cmd_topic, odom_topic, angular):
        """Publishes a turn on `cmd_topic` until the yaw on `odom_topic` changes by 0.5 rad."""
        name = f"{cmd_topic} -> {odom_topic}"
        self.pending[name] = True
        start = None
        cmd = Twist()
        cmd.angular.z = angular
        pub = self.create_publisher(Twist, cmd_topic, 10)
        timer = self.create_timer(0.1, lambda: pub.publish(cmd))

        def on_odom(msg):
            nonlocal start
            if name not in self.pending:
                return
            if start is None:
                start = yaw(msg)
                return
            turned = abs(math.remainder(yaw(msg) - start, math.tau))
            if turned > 0.5:
                del self.pending[name]
                timer.cancel()
                self.get_logger().info(f"ok   {name} (turned {turned:.2f} rad)")

        self.create_subscription(Odometry, odom_topic, on_odom, qos_profile_sensor_data)

    def run(self):
        deadline = time.monotonic() + DEADLINE_S
        while self.pending and time.monotonic() < deadline:
            rclpy.spin_once(self, timeout_sec=0.5)
        for name in self.pending:
            self.failures.append(f"{name}: nothing within {DEADLINE_S:.0f} s")
            self.get_logger().error(f"FAIL {name}: nothing within {DEADLINE_S:.0f} s")
        return not self.failures


def main():
    scenario = sys.argv[1] if len(sys.argv) > 1 else ""
    rclpy.init()
    node = Checker()
    if scenario == "sample":
        node.expect("/moqros/image", Image, image(1280, 720, "camera"))
        node.expect("/moqros/frames", Image, image(640, 480, "frames"))
        node.expect("/moqros/chatter", Header, header("chatter"))
        node.expect("/moqros/pose", PoseStamped, pose)
    elif scenario == "teleop":
        node.expect("/e2e/camera", Image, image(640, 360))
        node.expect("/e2e/map", Image, image(320, 320))
        node.expect_turn("/e2e/cmd_vel", "/e2e/odom", 1.0)
    else:
        sys.exit(f"usage: {sys.argv[0]} sample|teleop")
    ok = node.run()
    node.destroy_node()
    rclpy.shutdown()
    sys.exit(0 if ok else 1)


if __name__ == "__main__":
    main()
