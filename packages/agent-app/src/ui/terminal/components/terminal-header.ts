import { type Component, Text, truncateToWidth } from "@liuxuedeng/agent-core-tui";
import { theme } from "../theme/theme.ts";

// Five terminal rows, 57 columns. No fonts, images or graphics runtime required.
const AGENT = [
	" ███   ████ █████ █   █ █████",
	"█   █ █     █     ██  █   █  ",
	"█████ █ ███ ████  █ █ █   █  ",
	"█   █ █   █ █     █  ██   █  ",
	"█   █  ███  █████ █   █   █  ",
];
const CORE = [
	"      ████  ███  ████  █████",
	"     █     █   █ █   █ █    ",
	" ███ █     █   █ ████  ████ ",
	"     █     █   █ █  █  █    ",
	"      ████  ███  █   █ █████",
];

export class TerminalHeader implements Component {
	private expanded: boolean;
	private readonly metadata: () => string;
	private readonly instructions: string;
	constructor(metadata: () => string, instructions: string, expanded = false) {
		this.metadata = metadata;
		this.instructions = instructions;
		this.expanded = expanded;
	}
	setExpanded(expanded: boolean): void {
		this.expanded = expanded;
	}
	invalidate(): void {}
	render(width: number): string[] {
		if (width <= 0) return [];
		const logo =
			width >= 59
				? AGENT.map((row, i) => ` ${theme.fg("accent", row)}${theme.fg("text", CORE[i])}`)
				: [truncateToWidth(theme.fg("accent", " AGENT") + theme.fg("text", "-CORE"), width, "")];
		const body = theme.fg("muted", this.metadata()) + (this.expanded ? `\n\n${this.instructions}` : "");
		return [...logo, ...new Text(body, 1, 0).render(width)];
	}
}
