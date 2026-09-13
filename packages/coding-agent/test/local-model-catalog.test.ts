import { existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AuthStorage } from "../src/core/auth-storage.ts";
import { ModelRuntime } from "../src/core/model-runtime.ts";

// The Pi online model catalog overlay was removed: built-in providers expose
// only their static local catalog, and no refresh path may contact the removed
// catalog service, even when networking is enabled and a stale overlay cache
// exists on disk.
describe("local-only model catalogs", () => {
	let tempDir: string;

	beforeEach(() => {
		tempDir = join(tmpdir(), `pi-test-local-catalog-${Date.now()}-${Math.random().toString(36).slice(2)}`);
		mkdirSync(tempDir, { recursive: true });
	});

	afterEach(() => {
		vi.restoreAllMocks();
		if (tempDir && existsSync(tempDir)) rmSync(tempDir, { recursive: true, force: true });
	});

	function seedLegacyOverlayCache(modelsStorePath: string): void {
		// A models-store.json shaped like the ones the removed pi.dev overlay wrote.
		writeFileSync(
			modelsStorePath,
			JSON.stringify({
				anthropic: {
					models: [
						{
							id: "overlay-only-model",
							name: "Overlay Only",
							api: "anthropic-messages",
							provider: "anthropic",
							baseUrl: "https://api.anthropic.com",
							reasoning: true,
							input: ["text"],
							cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
							contextWindow: 1000,
							maxTokens: 100,
						},
					],
					checkedAt: Date.now(),
					lastModified: Date.now() + 1_000_000,
				},
			}),
		);
	}

	it("creates with network refresh enabled without contacting the removed catalog service", async () => {
		const fetchSpy = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("unexpected network request"));
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory(),
			modelsStorePath: join(tempDir, "models-store.json"),
			modelsPath: null,
			allowModelNetwork: true,
		});

		expect(runtime.getModel("anthropic", "claude-haiku-4-5")).toBeDefined();
		expect(runtime.getModel("anthropic", "overlay-only-model")).toBeUndefined();
		expect(fetchSpy).not.toHaveBeenCalled();
	});

	it("keeps the local catalog when a legacy overlay cache exists on disk", async () => {
		const fetchSpy = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("unexpected network request"));
		const modelsStorePath = join(tempDir, "models-store.json");
		seedLegacyOverlayCache(modelsStorePath);
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory(),
			modelsStorePath,
			modelsPath: null,
			allowModelNetwork: true,
		});

		expect(runtime.getModel("anthropic", "claude-haiku-4-5")).toBeDefined();
		expect(runtime.getModel("anthropic", "overlay-only-model")).toBeUndefined();
		expect(fetchSpy).not.toHaveBeenCalled();
	});

	it("does not contact the removed catalog service on a forced manual refresh", async () => {
		const fetchSpy = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("unexpected network request"));
		const modelsStorePath = join(tempDir, "models-store.json");
		seedLegacyOverlayCache(modelsStorePath);
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory(),
			modelsStorePath,
			modelsPath: null,
		});

		await runtime.refresh({ allowNetwork: true, force: true });

		expect(runtime.getModel("anthropic", "claude-haiku-4-5")).toBeDefined();
		expect(runtime.getModel("anthropic", "overlay-only-model")).toBeUndefined();
		expect(fetchSpy).not.toHaveBeenCalled();
	});

	it("keeps FileModelsStore-backed custom model data intact across refresh", async () => {
		vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("unexpected network request"));
		const modelsPath = join(tempDir, "models.json");
		writeFileSync(
			modelsPath,
			JSON.stringify({
				providers: {
					"custom-provider": {
						name: "Custom Provider",
						baseUrl: "https://custom.example.com/v1",
						apiKey: "$CUSTOM_TEST_API_KEY",
						api: "openai-completions",
						models: [
							{
								id: "custom-model",
								name: "Custom Model",
								reasoning: false,
								input: ["text"],
								cost: { input: 1, output: 2, cacheRead: 0, cacheWrite: 0 },
								contextWindow: 128000,
								maxTokens: 8192,
							},
						],
					},
				},
			}),
		);
		const runtime = await ModelRuntime.create({
			credentials: AuthStorage.inMemory(),
			modelsStorePath: join(tempDir, "models-store.json"),
			modelsPath,
			allowModelNetwork: true,
		});

		expect(runtime.getModel("custom-provider", "custom-model")).toMatchObject({
			baseUrl: "https://custom.example.com/v1",
			api: "openai-completions",
		});

		await runtime.refresh({ allowNetwork: true, force: true });
		expect(runtime.getModel("custom-provider", "custom-model")).toBeDefined();
	});
});
