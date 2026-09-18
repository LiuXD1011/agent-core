import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { PACKAGE_NAME, VERSION } from "../src/config.ts";
import { getSelfUpdatePlan } from "../src/package-manager-cli.ts";

const originalOffline = process.env.AGENT_CORE_OFFLINE;

beforeEach(() => {
	delete process.env.AGENT_CORE_OFFLINE;
});

afterEach(() => {
	vi.unstubAllGlobals();
	vi.unstubAllEnvs();
	if (originalOffline === undefined) {
		delete process.env.AGENT_CORE_OFFLINE;
	} else {
		process.env.AGENT_CORE_OFFLINE = originalOffline;
	}
});

describe("self-update branding", () => {
	it("refuses to run when no version source is configured and suggests the npm upgrade path", async () => {
		delete process.env.AGENT_CORE_VERSION_CHECK_URL;
		const fetchMock = vi.fn();
		vi.stubGlobal("fetch", fetchMock);

		await expect(getSelfUpdatePlan(false)).rejects.toThrow(/npm install -g @liuxuedeng\/agent-core@latest/);
		// No version request and therefore no package-manager write can happen.
		expect(fetchMock).not.toHaveBeenCalled();
	});

	it("builds the self-update target from the current product only", async () => {
		vi.stubEnv("AGENT_CORE_VERSION_CHECK_URL", "https://example.invalid/latest");
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: "999.0.0" })),
		);

		const plan = await getSelfUpdatePlan(false);
		expect(plan.packageName).toBe(PACKAGE_NAME);
		expect(plan.packageName).toBe("@liuxuedeng/agent-core");
		expect(plan.installSpec).toBe(`@liuxuedeng/agent-core@999.0.0`);
		expect(plan.shouldRun).toBe(true);
	});

	it("refuses to switch products when the feed serves a different package", async () => {
		vi.stubEnv("AGENT_CORE_VERSION_CHECK_URL", "https://example.invalid/latest");
		const fetchMock = vi.fn(async () =>
			Response.json({ packageName: "@earendil-works/pi-coding-agent", version: "999.0.0" }),
		);
		vi.stubGlobal("fetch", fetchMock);

		await expect(getSelfUpdatePlan(false)).rejects.toThrow(/拒绝安装其他产品/);
		expect(fetchMock).toHaveBeenCalledOnce();
	});

	it("does not update when the feed reports the current version", async () => {
		vi.stubEnv("AGENT_CORE_VERSION_CHECK_URL", "https://example.invalid/latest");
		vi.stubGlobal(
			"fetch",
			vi.fn(async () => Response.json({ version: VERSION })),
		);

		const plan = await getSelfUpdatePlan(false);
		expect(plan.shouldRun).toBe(false);
		expect(plan.packageName).toBe(PACKAGE_NAME);
	});
});
