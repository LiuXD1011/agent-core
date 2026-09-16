import { spawnSync } from "node:child_process";
import { describe, expect, it } from "vitest";
import { mergeProviderAttributionHeaders } from "../src/app/provider-attribution.ts";
import { applyProcessMarkers } from "../src/cli/process-markers.ts";
import { setupCli } from "../src/cli/setup.ts";
import { APP_NAME } from "../src/config.ts";

function modelStub(provider: string, baseUrl: string): Parameters<typeof mergeProviderAttributionHeaders>[0] {
	return { provider, baseUrl } as Parameters<typeof mergeProviderAttributionHeaders>[0];
}

const OPENROUTER_URL = "https://openrouter.ai/api/v1";
const NVIDIA_URL = "https://integrate.api.nvidia.com/v1";
const CLOUDFLARE_URL = "https://api.cloudflare.com/client/v4";
const OPENCODE_URL = "https://opencode.ai/zen/v1";

describe("process markers identify Agent Core in both entry points", () => {
	it("sets AI_AGENT=agent-core and AGENT_CORE_CODING_AGENT=true via the CLI setup", () => {
		setupCli();
		expect(process.env.AI_AGENT).toBe("agent-core");
		expect(process.env.AGENT_CORE_CODING_AGENT).toBe("true");
		expect(APP_NAME).toBe("agent-core");
	});

	it("sets the same markers through the shared helper used by the RPC entry", async () => {
		applyProcessMarkers("agent-core-rpc");
		expect(process.env.AI_AGENT).toBe("agent-core");
		expect(process.env.AGENT_CORE_CODING_AGENT).toBe("true");
	});

	it("rpc entry point runs end to end and reports the Agent Core version", () => {
		const result = spawnSync(process.execPath, ["src/rpc-entry.ts", "--version"], {
			cwd: new URL("..", import.meta.url).pathname,
			encoding: "utf8",
			env: { ...process.env, AGENT_CORE_OFFLINE: "1" },
			timeout: 60000,
		});
		expect(result.status).toBe(0);
		expect(result.stdout.trim()).toMatch(/^\d+\.\d+\.\d+/);
	});
});

describe("provider headers", () => {
	it.each([
		["openrouter", OPENROUTER_URL],
		["nvidia", NVIDIA_URL],
		["cloudflare-workers-ai", CLOUDFLARE_URL],
	])("does not inject optional attribution for %s", (provider, url) => {
		expect(mergeProviderAttributionHeaders(modelStub(provider, url), undefined)).toBeUndefined();
	});
	it("keeps OpenCode session protocol headers", () => {
		expect(mergeProviderAttributionHeaders(modelStub("opencode", OPENCODE_URL), "session-123")).toEqual({
			"x-opencode-session": "session-123",
			"x-opencode-client": "agent-core",
		});
	});
	it("keeps explicit provider headers with request overrides", () => {
		expect(
			mergeProviderAttributionHeaders(
				modelStub("openrouter", OPENROUTER_URL),
				undefined,
				{ "X-OpenRouter-Title": "provider", "HTTP-Referer": "https://example.test" },
				{ "X-OpenRouter-Title": "request" },
			),
		).toEqual({ "X-OpenRouter-Title": "request", "HTTP-Referer": "https://example.test" });
	});
});
