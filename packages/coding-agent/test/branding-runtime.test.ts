import { spawnSync } from "node:child_process";
import { afterEach, describe, expect, it } from "vitest";
import { setupCli } from "../src/cli/setup.ts";
import { APP_NAME, getShareViewerUrl } from "../src/config.ts";
import { mergeProviderAttributionHeaders } from "../src/core/provider-attribution.ts";
import type { SettingsManager } from "../src/core/settings-manager.ts";

function settingsStub(enableInstallTelemetry: boolean): SettingsManager {
	return { getEnableInstallTelemetry: () => enableInstallTelemetry } as unknown as SettingsManager;
}

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
		const { applyProcessMarkers } = await import("../src/cli/process-markers.ts");
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

describe("share viewer URL", () => {
	const originalViewerUrl = process.env.AGENT_CORE_SHARE_VIEWER_URL;

	afterEach(() => {
		if (originalViewerUrl === undefined) {
			delete process.env.AGENT_CORE_SHARE_VIEWER_URL;
		} else {
			process.env.AGENT_CORE_SHARE_VIEWER_URL = originalViewerUrl;
		}
	});

	it("defaults to the upstream Pi share viewer, which is an external service", () => {
		delete process.env.AGENT_CORE_SHARE_VIEWER_URL;
		expect(getShareViewerUrl("gist-123")).toBe("https://pi.dev/session/#gist-123");
	});

	it("can be overridden to a Agent Core-controlled viewer", () => {
		process.env.AGENT_CORE_SHARE_VIEWER_URL = "https://example.invalid/viewer/";
		expect(getShareViewerUrl("gist-123")).toBe("https://example.invalid/viewer/#gist-123");
	});
});

describe("provider attribution headers identify Agent Core", () => {
	const enabled = settingsStub(true);
	const disabled = settingsStub(false);

	afterEach(() => {
		delete process.env.AGENT_CORE_TELEMETRY;
	});

	it("OpenRouter attribution uses the Agent Core repository and name", () => {
		const headers = mergeProviderAttributionHeaders(modelStub("openrouter", OPENROUTER_URL), enabled, undefined);
		expect(headers).toMatchObject({
			"HTTP-Referer": "https://github.com/LiuXD1011/agent-core",
			"X-OpenRouter-Title": "agent-core",
			"X-OpenRouter-Categories": "cli-agent",
		});
	});

	it("no automatic NVIDIA attribution headers are injected", () => {
		const headers = mergeProviderAttributionHeaders(modelStub("nvidia", NVIDIA_URL), enabled, undefined);
		expect(headers).toBeUndefined();
	});

	it("Cloudflare client identity is agent-core", () => {
		const headers = mergeProviderAttributionHeaders(
			modelStub("cloudflare-workers-ai", CLOUDFLARE_URL),
			enabled,
			undefined,
		);
		expect(headers).toEqual({ "User-Agent": "agent-core" });
	});

	it("OpenCode session headers keep the session ID and use the agent-core client id", () => {
		const headers = mergeProviderAttributionHeaders(modelStub("opencode", OPENCODE_URL), disabled, "session-123");
		expect(headers).toEqual({ "x-opencode-session": "session-123", "x-opencode-client": "agent-core" });
	});

	it("no attribution headers are sent when telemetry is disabled and no session applies", () => {
		expect(
			mergeProviderAttributionHeaders(modelStub("openrouter", OPENROUTER_URL), disabled, undefined),
		).toBeUndefined();
		expect(
			mergeProviderAttributionHeaders(modelStub("anthropic", "https://api.anthropic.com"), enabled, undefined),
		).toBeUndefined();
	});

	it("AGENT_CORE_TELEMETRY overrides the settings value in both directions", () => {
		process.env.AGENT_CORE_TELEMETRY = "0";
		expect(
			mergeProviderAttributionHeaders(modelStub("openrouter", OPENROUTER_URL), enabled, undefined),
		).toBeUndefined();

		process.env.AGENT_CORE_TELEMETRY = "1";
		expect(
			mergeProviderAttributionHeaders(modelStub("openrouter", OPENROUTER_URL), disabled, undefined),
		).toMatchObject({
			"X-OpenRouter-Title": "agent-core",
		});
	});

	it("explicit provider headers take priority over default attribution headers", () => {
		const headers = mergeProviderAttributionHeaders(modelStub("openrouter", OPENROUTER_URL), enabled, undefined, {
			"X-OpenRouter-Title": "my-custom-title",
		});
		expect(headers).toMatchObject({
			"HTTP-Referer": "https://github.com/LiuXD1011/agent-core",
			"X-OpenRouter-Title": "my-custom-title",
		});
	});
});
