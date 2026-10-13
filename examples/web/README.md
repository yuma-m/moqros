# moqros web viewer

A React and TypeScript app built on [`@moq/watch`](https://www.npmjs.com/package/@moq/watch). It opens one
connection to the relay and shows any number of broadcasts side by side: a video panel for each image
broadcast, and a data panel with the latest message for each data broadcast. It tells them apart by the
broadcast's catalog.

A hosted copy, built from `main`, is at <https://yuma-m.github.io/moqros/>.

## Picking broadcasts

The sidebar lists every broadcast the relay announces. With *Show all announced* on (the default), each one
gets a panel; the filter narrows that to names containing some text, and unticking a broadcast hides it.
*Add* shows a broadcast by name, even one the relay doesn't announce, as `auto` (decided by the catalog),
`video`, or `data`.

## Query parameters

The view is kept in the URL, so it can be bookmarked or shared:

| Parameter | What |
| --- | --- |
| `url` | Relay URL. Defaults to `VITE_RELAY_URL` at build time, else `http://<page host>:4443/anon` (the hosted viewer uses `http://localhost:4443/anon`). |
| `auto=0` | Turn off *Show all announced*. |
| `filter` | Only show announced broadcasts whose name contains this text. |
| `show`, `video`, `data` | Comma-separated broadcasts to always show: by catalog, or as that kind. |
| `hide` | Comma-separated broadcasts to hide. |
| `teleop` | Broadcast to publish velocity commands on; see [Teleop](#teleop). |

The `name` parameter of the old viewer still works as `video`.

The address bar never shows the relay's token: the viewer drops a `jwt` query parameter or a JWT-shaped path
segment (as Cloudflare's relays use) from `url`, and keeps the full URL in the tab's session storage, so a
reload still connects. A link opened elsewhere asks for the token again. When a link brings a token in `url`,
the viewer takes it out of the address bar as soon as the page loads.

## Teleop

With `teleop=<broadcast>` (e.g. `?teleop=robot/cmd_vel`), or *Add* with *As* set to `teleop`, the viewer shows a pad that publishes
`geometry_msgs/msg/Twist` messages to the relay, as a moqros JSON data broadcast in snapshot mode. Up and down
set `linear.x`, and left and right set `angular.z`, up to the speeds on the sliders. While a direction is held
(on the pad, or with WASD or the arrow keys), it repeats the command every 100 ms. Releasing it, pressing Space,
or leaving the window sends one zero command.

Only a page that's visible and focused publishes; it retracts the broadcast when it goes to the background. A
relay keeps serving the first publisher of a name, so this lets the tab or window in use take over from others
left open. Bridge the broadcast onto ROS with `moqros-sub` or `moqros-bridge`
(`--broadcast robot/cmd_vel,topic=/cmd_vel`). The robot should stop on its own when commands stop coming, like the
[teleop demo](../../README.md#teleop-demo-docker)'s simulator does.

## Relays on other machines

The hosted viewer is served over HTTPS, so the browser only lets it reach an `http://` relay on `localhost`.
For a relay on another machine, give the relay a real certificate and use an `https://` URL. Some browsers
ask before a public site may reach a local address.

## Data renderers

JSON messages show as a tree or as text, and CDR messages as their size and a hex dump (the browser can't
decode CDR). Messages with a header (`std_msgs/msg/Header` and `*Stamped` types) can also show their stamp
and age on arrival. Add a view for a message type of your own by registering a React component in
`src/panels/renderers/index.ts`:

```tsx
register({ rosType: "sensor_msgs/msg/Imu", format: "json", label: "IMU", component: ImuView });
```

The component gets the broadcast name, the track (ROS type, format, mode), the raw payload, and the parsed
JSON message. Each panel lets you switch between the renderers that match its type.

## Development

Run the relay and the bridges (e.g. the [Docker sample](../../README.md#end-to-end-sample-docker), without its
`web` service), then:

```sh
npm install
npm run dev     # http://localhost:8080 with hot reload
npm run build   # type-check and build into dist/
```
