import { setKeybindings, type TUI } from "@liuxuedeng/agent-core-tui";
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { KeybindingsManager } from "../../../src/ui/keybindings.ts";
import { ModelSelectorComponent } from "../../../src/ui/terminal/components/model-selector.ts";
import { initTheme } from "../../../src/ui/terminal/theme/theme.ts";
import { stripAnsi } from "../../../src/utils/ansi.ts";
import { createHarness, type Harness } from "../harness.ts";

function createFakeTui(): TUI {
	return { requestRender: () => {} } as unknown as TUI;
}

/** Return the model id of the highlighted (→) row in the rendered selector. */
function selectedModelId(rendered: string): string | undefined {
	const line = rendered.split("\n").find((l) => l.startsWith("→ "));
	if (!line) return undefined;
	const rest = line.replace(/^→\s*/, "");
	const id = rest.split(" [")[0]?.replace(/^✓\s*/, "");
	return id?.trim() || undefined;
}

describe("model selector filter resets selection to top", () => {
	const harnesses: Harness[] = [];

	beforeAll(() => {
		initTheme("dark");
	});

	beforeEach(() => {
		setKeybindings(new KeybindingsManager());
	});

	afterAll(() => {
		while (harnesses.length > 0) {
			harnesses.pop()?.cleanup();
		}
	});

	it("moves selection to the first row when typing a query", async () => {
		const harness = await createHarness({
			models: [
				{ id: "alpha-1", name: "Alpha One", reasoning: true },
				{ id: "alpha-2", name: "Alpha Two", reasoning: true },
				{ id: "alpha-3", name: "Alpha Three", reasoning: true },
				{ id: "beta-1", name: "Beta One", reasoning: true },
			],
		});
		harnesses.push(harness);

		const current = harness.getModel("alpha-1")!;
		const selector = new ModelSelectorComponent(
			createFakeTui(),
			current,
			harness.session.modelRuntime,
			() => {},
			() => {},
		);

		await vi.waitFor(() => {
			const rendered = stripAnsi(selector.render(120).join("\n"));
			expect(rendered).toContain("Model catalogs refreshed.");
		});

		// Current model (alpha-1) is sorted first, so selection starts on row 0.
		expect(selectedModelId(stripAnsi(selector.render(120).join("\n")))).toBe("alpha-1");

		// Move selection down two rows to alpha-3.
		selector.handleInput("\x1b[B");
		selector.handleInput("\x1b[B");
		expect(selectedModelId(stripAnsi(selector.render(120).join("\n")))).toBe("alpha-3");

		// Type a query that matches the three alpha models. The selection must
		// move back to the top row (alpha-1), not stay clamped at index 2.
		for (const char of "alpha") {
			selector.handleInput(char);
		}

		const rendered = stripAnsi(selector.render(120).join("\n"));
		expect(selectedModelId(rendered)).toBe("alpha-1");
		// Sanity: the filter actually narrowed the list.
		expect(rendered).not.toContain("beta-1");
	});
});
