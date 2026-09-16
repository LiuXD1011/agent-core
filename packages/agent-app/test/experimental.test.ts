import { afterEach, describe, expect, it } from "vitest";
import { areExperimentalFeaturesEnabled } from "../src/app/experimental.ts";

describe("areExperimentalFeaturesEnabled", () => {
	const originalPiExperimental = process.env.AGENT_CORE_EXPERIMENTAL;

	afterEach(() => {
		if (originalPiExperimental === undefined) {
			delete process.env.AGENT_CORE_EXPERIMENTAL;
		} else {
			process.env.AGENT_CORE_EXPERIMENTAL = originalPiExperimental;
		}
	});

	it("returns false when AGENT_CORE_EXPERIMENTAL is unset", () => {
		delete process.env.AGENT_CORE_EXPERIMENTAL;

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});

	it("returns false when AGENT_CORE_EXPERIMENTAL is empty", () => {
		process.env.AGENT_CORE_EXPERIMENTAL = "";

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});

	it("returns true when AGENT_CORE_EXPERIMENTAL is set to 1", () => {
		process.env.AGENT_CORE_EXPERIMENTAL = "1";

		expect(areExperimentalFeaturesEnabled()).toBe(true);
	});

	it("returns false when AGENT_CORE_EXPERIMENTAL is set to 0", () => {
		process.env.AGENT_CORE_EXPERIMENTAL = "0";

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});

	it("returns false when AGENT_CORE_EXPERIMENTAL is set to a non-1 value", () => {
		process.env.AGENT_CORE_EXPERIMENTAL = "true";

		expect(areExperimentalFeaturesEnabled()).toBe(false);
	});
});
