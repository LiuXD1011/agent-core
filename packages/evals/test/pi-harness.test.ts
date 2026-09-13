import { describe, expect, it } from "vitest";
import { resolveModelSelection } from "../src/pi-harness.ts";

describe("resolveModelSelection", () => {
	it("prefers an explicit harness model over environment defaults", () => {
		expect(
			resolveModelSelection(
				{ provider: "anthropic", id: "claude-opus-4-6" },
				{ AGENT_CORE_PROVIDER: "openai-codex", AGENT_CORE_MODEL: "gpt-5.6-sol" },
			),
		).toEqual({ provider: "anthropic", id: "claude-opus-4-6" });
	});

	it("uses trimmed environment defaults when the harness has no explicit model", () => {
		expect(
			resolveModelSelection(undefined, { AGENT_CORE_PROVIDER: " openai-codex ", AGENT_CORE_MODEL: " gpt-5.6-sol " }),
		).toEqual({
			provider: "openai-codex",
			id: "gpt-5.6-sol",
		});
	});

	it.each([
		[undefined, {}],
		[undefined, { AGENT_CORE_PROVIDER: "openai-codex" }],
		[undefined, { AGENT_CORE_MODEL: "gpt-5.6-sol" }],
		[
			{ provider: "", id: "gpt-5.6-sol" },
			{ AGENT_CORE_PROVIDER: "openai-codex", AGENT_CORE_MODEL: "gpt-5.6-sol" },
		],
	] as const)("rejects an incomplete model selection", (explicitModel, environment) => {
		expect(() => resolveModelSelection(explicitModel, environment)).toThrow(
			"Select a harness model explicitly or set both AGENT_CORE_PROVIDER and AGENT_CORE_MODEL as defaults.",
		);
	});
});
