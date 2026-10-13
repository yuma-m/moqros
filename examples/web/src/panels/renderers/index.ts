// Built-in renderers. Register your own here (or anywhere before the first render) with
// `register`; see `registry.ts`.
import { CdrBytes } from "./CdrBytes";
import { Stamped } from "./Stamped";
import { JsonText, JsonTree } from "./JsonTree";
import { Odometry } from "./Odometry";
import { register } from "./registry";

register({ rosType: "*", format: "cdr", label: "Bytes", component: CdrBytes });
register({ rosType: "*", format: "json", label: "Text", component: JsonText });
register({ rosType: "*", format: "json", label: "Tree", component: JsonTree });
// std_msgs/msg/Header itself, and the *Stamped messages that carry one.
register({ rosType: "nav_msgs/msg/Odometry", format: "json", label: "Odometry", component: Odometry });
register({ rosType: /^std_msgs\/msg\/Header$|Stamped$/, format: "json", label: "Stamp", component: Stamped });

export { register, renderersFor, type RendererProps } from "./registry";
