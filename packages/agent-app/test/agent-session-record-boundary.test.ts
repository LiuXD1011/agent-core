import { mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fauxAssistantMessage, fauxProvider } from "@liuxuedeng/agent-core-ai/providers/faux";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { AgentSessionEvent } from "../src/app/application.ts";
import { DefaultResourceLoader } from "../src/app/resource-loader.ts";
import { SettingsManager } from "../src/app/settings-manager.ts";
import { createAgentSession } from "../src/sdk.ts";

describe("session record boundary", () => {
	let directory: string;

	beforeEach(() => {
		directory = mkdtempSync(join(tmpdir(), "agent-record-boundary-"));
		mkdirSync(join(directory, "agent"), { recursive: true });
	});

	afterEach(() => {
		rmSync(directory, { recursive: true, force: true });
	});

	it("commits records before notifying observers, isolates failures and tampering", async () => {
		const faux = fauxProvider({
			provider: "record-boundary",
			api: "record-boundary-api",
			models: [{ id: "m", contextWindow: 8_192, maxTokens: 1_024 }],
		});
		faux.setResponses([fauxAssistantMessage("done")]);

		const settingsManager = SettingsManager.inMemory({ retry: { enabled: false } });
		const resourceLoader = new DefaultResourceLoader({
			cwd: directory,
			agentDir: join(directory, "agent"),
			settingsManager,
			noExtensions: true,
			noSkills: true,
			noPromptTemplates: true,
			noThemes: true,
			noContextFiles: true,
			systemPrompt: "Deterministic record boundary test.",
			extensionFactories: [
				{
					name: "record-boundary-provider",
					factory: (pi) => {
						pi.registerProvider(faux.provider);
					},
				},
			],
		});
		await resourceLoader.reload();

		const { session } = await createAgentSession({
			cwd: directory,
			agentDir: join(directory, "agent"),
			model: faux.getModel(),
			thinkingLevel: "off",
			resourceLoader,
			settingsManager,
		});

		const observations: { type: string; persistedAtNotify: boolean }[] = [];

		session.subscribe((event: AgentSessionEvent) => {
			if (event.type !== "message_end") return;
			// The committed session entry must already exist when observers run.
			const serialized = JSON.stringify(event.message);
			const persisted = session.sessionManager
				.getEntries()
				.some((entry) => entry.type === "message" && JSON.stringify(entry.message) === serialized);
			observations.push({ type: event.message.role, persistedAtNotify: persisted });
		});

		// A mutating observer cannot rewrite the committed record...
		session.subscribe((event: AgentSessionEvent) => {
			if (event.type === "message_end" && event.message.role === "assistant") {
				(event.message as { content: unknown }).content = [{ type: "text", text: "tampered" }];
			}
		});
		// ...and a throwing observer must not break the run or persistence.
		session.subscribe(() => {
			throw new Error("observer exploded");
		});

		await session.prompt("hello", { expandPromptTemplates: false });

		expect(observations.length).toBeGreaterThan(0);
		expect(observations.every((observation) => observation.persistedAtNotify)).toBe(true);

		const messages = session.sessionManager
			.getEntries()
			.flatMap((entry) => (entry.type === "message" ? [entry.message] : []));
		const assistant = messages.find((message) => message.role === "assistant");
		const assistantText = JSON.stringify(assistant?.content);
		expect(assistantText).toContain("done");
		expect(assistantText).not.toContain("tampered");
		expect(session.getLastAssistantText()).toBe("done");
		expect(session.isIdle).toBe(true);

		session.dispose();
	});
});
