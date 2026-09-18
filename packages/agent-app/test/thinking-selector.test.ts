import { setKeybindings } from "@liuxuedeng/agent-core-tui";
import { beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { KeybindingsManager } from "../src/ui/keybindings.ts";
import { ThinkingSelectorComponent } from "../src/ui/terminal/components/thinking-selector.ts";
import { initTheme } from "../src/ui/terminal/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";

describe("thinking selector", () => {
	beforeAll(() => {
		initTheme("dark");
	});

	beforeEach(() => {
		setKeybindings(new KeybindingsManager());
	});

	it("keeps the current thinking level marked while browsing", () => {
		const selector = new ThinkingSelectorComponent(
			"medium",
			["medium", "high"],
			() => {},
			() => {},
		);
		const getLevelRow = (level: string): string | undefined =>
			selector
				.getSelectList()
				.render(80)
				.map((line) => stripAnsi(line))
				.find((line) => line.includes(level));

		expect(selector.getSelectList().getSelectedItem()?.label).toBe("✓ 中");
		expect(getLevelRow("中")?.startsWith("→ ✓ 中")).toBe(true);
		selector.handleInput("\x1b[B");
		expect(getLevelRow("中")?.startsWith("  ✓ 中")).toBe(true);
		expect(getLevelRow("高")?.startsWith("→   高")).toBe(true);
	});

	it("uses the configured save binding", () => {
		setKeybindings(new KeybindingsManager({ "app.thinking.save": "ctrl+r" }));
		const saveDefault = vi.fn();
		const selector = new ThinkingSelectorComponent(
			"medium",
			["medium", "high"],
			() => {},
			() => {},
			saveDefault,
		);

		expect(stripAnsi(selector.render(80).join("\n"))).toContain("Ctrl+R 设为默认");
		selector.handleInput("\x13");
		expect(saveDefault).not.toHaveBeenCalled();
		selector.handleInput("\x12");
		expect(saveDefault).toHaveBeenCalledWith("medium");
	});
});
