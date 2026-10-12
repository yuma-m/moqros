import type { ReactNode } from "react";

/** Frame shared by every panel: title, status line, toolbar, and a close button. */
export function Panel(props: {
	name: string;
	kind: string;
	status: ReactNode;
	tools?: ReactNode;
	onClose: () => void;
	children?: ReactNode;
}) {
	return (
		<section className={`panel panel-${props.kind}`}>
			<header>
				<h2 title={props.name}>{props.name}</h2>
				<div className="tools">
					{props.tools}
					<button type="button" className="icon" title="Remove panel" onClick={props.onClose}>
						×
					</button>
				</div>
			</header>
			<div className="status">{props.status}</div>
			{props.children}
		</section>
	);
}
