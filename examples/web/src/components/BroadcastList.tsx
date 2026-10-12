/** The broadcasts the relay announces, each with a checkbox to show or hide its panel. */
export function BroadcastList(props: {
	announced: string[];
	shown: Set<string>;
	auto: boolean;
	filter: string;
	onAuto: (auto: boolean) => void;
	onFilter: (filter: string) => void;
	onToggle: (name: string, show: boolean) => void;
}) {
	return (
		<aside className="broadcasts">
			<div className="broadcasts-head">
				<label className="inline" title="Show a panel for every announced broadcast">
					<input type="checkbox" checked={props.auto} onChange={(event) => props.onAuto(event.target.checked)} />
					Show all announced
				</label>
				<input
					className="filter"
					value={props.filter}
					onChange={(event) => props.onFilter(event.target.value)}
					placeholder="filter"
					spellCheck={false}
				/>
			</div>
			{props.announced.length === 0 ? (
				<div className="hint">No broadcasts announced yet.</div>
			) : (
				<ul>
					{props.announced.map((name) => (
						<li key={name}>
							<label className="inline">
								<input
									type="checkbox"
									checked={props.shown.has(name)}
									onChange={(event) => props.onToggle(name, event.target.checked)}
								/>
								{name}
							</label>
						</li>
					))}
				</ul>
			)}
		</aside>
	);
}
