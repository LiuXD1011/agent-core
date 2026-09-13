import { arch, platform, release } from "node:os";
import { describe, expect, it } from "vitest";
import { getPiUserAgent } from "../src/utils/pi-user-agent.ts";

describe("getPiUserAgent", () => {
	it("uses the agent-core brand with runtime OS info in Node", () => {
		expect(getPiUserAgent()).toBe(`agent-core (${platform()} ${release()}; ${arch()})`);
	});
});
