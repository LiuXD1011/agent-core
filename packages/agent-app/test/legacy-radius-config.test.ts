import { mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { InMemoryModelsStore } from "@liuxuedeng/agent-core-ai";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AuthStorage } from "../src/app/auth-storage.ts";
import { ModelRuntime } from "../src/app/model-runtime.ts";

// Regression for the Radius gateway removal: legacy Radius configuration must
// surface an understandable configuration error without network activity or a
// silent switch to another provider.
describe("legacy Radius configuration", () => {
	let tempDir: string;

	beforeEach(() => {
		tempDir = join(tmpdir(), `pi-test-legacy-radius-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
	});

	afterEach(() => {
		vi.restoreAllMocks();
		if (tempDir) rmSync(tempDir, { recursive: true, force: true });
	});

	it("does not register the removed built-in Radius provider", async () => {
		const fetchSpy = vi.spyOn(globalThis, "fetch");
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory({
				radius: {
					type: "oauth",
					access: "legacy-access",
					refresh: "legacy-refresh",
					expires: Date.now() + 60 * 60_000,
				},
			}),
			modelsStore: new InMemoryModelsStore(),
			modelsPath: null,
		});

		expect(runtime.getProvider("radius")).toBeUndefined();
		expect(runtime.getModels("radius")).toEqual([]);
		expect(runtime.hasConfiguredAuth("radius")).toBe(false);
		expect(fetchSpy).not.toHaveBeenCalled();
	});

	it("treats a legacy models.json oauth provider as a plain custom provider without network use", async () => {
		const fetchSpy = vi.spyOn(globalThis, "fetch");
		const modelsPath = join(tempDir, "models.json");
		writeFileSync(
			modelsPath,
			JSON.stringify({
				providers: {
					"radius-dev": { name: "Radius (dev)", baseUrl: "https://radius.example.com", oauth: "radius" },
				},
			}),
		);
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory(),
			modelsStore: new InMemoryModelsStore(),
			modelsPath,
			allowModelNetwork: true,
		});

		// The unknown "oauth" key is ignored; the entry degrades to a model-less
		// custom provider that would prompt for an API key. It must never reach
		// the network on its own or fall back to another provider's models.
		expect(runtime.getProvider("radius-dev")?.name).toBe("Radius (dev)");
		expect(runtime.getModels("radius-dev")).toEqual([]);
		expect(runtime.getError()).toBeUndefined();
		expect(fetchSpy).not.toHaveBeenCalled();
	});

	it("reports a configuration error for a legacy oauth-only provider without baseUrl", async () => {
		const fetchSpy = vi.spyOn(globalThis, "fetch");
		const modelsPath = join(tempDir, "models.json");
		writeFileSync(modelsPath, JSON.stringify({ providers: { "radius-dev": { oauth: "radius" } } }));
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory(),
			modelsStore: new InMemoryModelsStore(),
			modelsPath,
			allowModelNetwork: true,
		});

		expect(runtime.getProvider("radius-dev")).toBeUndefined();
		expect(runtime.getError()).toContain("radius-dev");
		expect(runtime.getError()).toContain("baseUrl");
		expect(fetchSpy).not.toHaveBeenCalled();
	});
});
