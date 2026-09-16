import { mkdir, mkdtemp, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Context } from "@liuxuedeng/agent-core-ai";
import { fauxAssistantMessage, fauxProvider, fauxToolCall } from "@liuxuedeng/agent-core-ai/providers/faux";
import { afterEach, describe, expect, it } from "vitest";
import type { AgentSession } from "../../src/app/application.ts";
import { DefaultResourceLoader } from "../../src/app/resource-loader.ts";
import { type Settings, SettingsManager } from "../../src/app/settings-manager.ts";
import { CONFIG_DIR_NAME } from "../../src/config.ts";
import { DEFAULT_CONFIG, loadContextConfig } from "../../src/context/config.ts";
import type { ExtensionFactory } from "../../src/extensions/types.ts";
import { type CreateAgentSessionOptions, createAgentSession } from "../../src/sdk.ts";
import { SessionManager } from "../../src/session/store.ts";
import { createHarness } from "../suite/harness.ts";

const roots: string[] = [];
const sessions: AgentSession[] = [];
const ALL = {
	...DEFAULT_CONFIG,
	mutationCommands: true,
	resultReferences: true,
	logReduction: true,
	boundaryCompaction: true,
};
afterEach(async () => {
	for (const session of sessions.splice(0)) session.dispose();
	for (const root of roots.splice(0)) await rm(root, { recursive: true, force: true });
});
async function setup(
	options: {
		projectTrusted?: boolean;
		projectConfig?: boolean;
		noExtensions?: boolean;
		settings?: Partial<Settings>;
		factory?: ExtensionFactory;
		sessionOptions?: Pick<CreateAgentSessionOptions, "tools" | "excludeTools" | "noTools" | "customTools">;
	} = {},
) {
	const cwd = await mkdtemp(join(tmpdir(), "agent-core-context-native-"));
	roots.push(cwd);
	const agentDir = join(cwd, "agent");
	await mkdir(agentDir);
	const configDir = options.projectConfig ? join(cwd, CONFIG_DIR_NAME) : agentDir;
	await mkdir(configDir, { recursive: true });
	await writeFile(join(configDir, "context.json"), JSON.stringify(ALL));
	const faux = fauxProvider({
		provider: "native-context-test",
		api: "native-context-test",
		models: [{ id: "native", contextWindow: 8_192, maxTokens: 1_024 }],
	});
	const settingsManager = SettingsManager.inMemory(
		{
			compaction: { enabled: false, keepRecentTokens: 150, reserveTokens: 1_024 },
			retry: { enabled: false },
			...options.settings,
		},
		{ projectTrusted: options.projectTrusted ?? false },
	);
	const loader = new DefaultResourceLoader({
		cwd,
		agentDir,
		settingsManager,
		noExtensions: options.noExtensions,
		noSkills: true,
		noThemes: true,
		noPromptTemplates: true,
		noContextFiles: true,
		systemPrompt: "Deterministic offline test. Preserve verification evidence.",
		extensionFactories: [
			{
				name: "native-provider",
				factory: (pi) => {
					pi.registerProvider(faux.provider);
					options.factory?.(pi);
				},
			},
		],
	});
	await loader.reload();
	const manager = SessionManager.create(cwd, join(agentDir, "sessions"));
	const { session } = await createAgentSession({
		cwd,
		agentDir,
		model: faux.getModel(),
		thinkingLevel: "off",
		resourceLoader: loader,
		sessionManager: manager,
		settingsManager,
		...options.sessionOptions,
	});
	sessions.push(session);
	const errors: unknown[] = [];
	await session.bindExtensions({ onError: (error) => errors.push(error) });
	return { cwd, agentDir, loader, manager, session, faux, errors };
}
function textOf(context: Context): string {
	return context.messages
		.map((message) =>
			typeof message.content === "string"
				? message.content
				: message.content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("\n"),
		)
		.join("\n");
}
function call(name: string, args: Record<string, unknown>) {
	return fauxAssistantMessage(fauxToolCall(name, args), { stopReason: "toolUse" });
}

import { ARCHIVE_TAIL_CUSTOM_TYPE, collectContextArchives, contextRuntimeRoot } from "../../src/session/artifacts.ts";
import { loadEntriesFromFile } from "../../src/session/store.ts";

describe("Session context, persistence and export", () => {
	it("ordinary sessions install context and tool policies by default", async () => {
		const cwd = await mkdtemp(join(tmpdir(), "codex-fresh-"));
		roots.push(cwd);
		const config = loadContextConfig(cwd, join(cwd, "empty-agent"));
		expect([
			config.mutationCommands,
			config.resultReferences,
			config.logReduction,
			config.boundaryCompaction,
		]).toEqual([true, true, true, true]);
	});
	it("public JSONL export carries recall archives by default", async () => {
		const { session, manager, faux, cwd } = await setup();
		faux.setResponses([fauxAssistantMessage("done")]);
		await session.prompt("hello", { expandPromptTemplates: false });
		const path = join(contextRuntimeRoot(manager)!, "observation-pack", "objects");
		await mkdir(path, { recursive: true });
		await writeFile(join(path, `obs_${"a".repeat(24)}.txt`), "evidence");
		const output = session.exportToJsonl(join(cwd, "export.jsonl"));
		expect(
			loadEntriesFromFile(output).some((e) => e.type === "custom" && e.customType === ARCHIVE_TAIL_CUSTOM_TYPE),
		).toBe(true);
	});
	it("oversized archives fail explicitly instead of silently dropping evidence", async () => {
		const { manager } = await setup();
		const path = join(contextRuntimeRoot(manager)!, "observation-pack", "objects");
		await mkdir(path, { recursive: true });
		const file = `obs_${"a".repeat(24)}.txt`;
		await writeFile(join(path, file), Buffer.alloc(4 * 1024 * 1024 + 1, 65));
		expect(() => collectContextArchives(manager)).toThrow("exceeds export size limits");
	});
	it("turn_end observer cannot rewrite authoritative history", async () => {
		const { session, manager, faux } = await setup();
		faux.setResponses([fauxAssistantMessage("done")]);
		session.subscribe((event) => {
			if (event.type === "turn_end" && event.message.role === "assistant") {
				const text = event.message.content.find((block) => block.type === "text");
				if (text?.type === "text") text.text = "tampered";
			}
		});
		await session.prompt("hello", { expandPromptTemplates: false });
		const assistant = manager.getEntries().find((e) => e.type === "message" && e.message.role === "assistant");
		expect(JSON.stringify(assistant)).not.toContain("tampered");
		expect(session.getLastAssistantText()).toBe("done");
	});
	for (const check of ["next-request", "bookkeeping"]) {
		it(`successful boundary compaction updates ${check}`, async () => {
			const marker = "CODEX_COMPACTION_CHECKPOINT";
			const { session, faux, manager } = await setup({
				factory: (pi) => {
					pi.on("session_before_compact", (event) => ({
						compaction: {
							summary: marker,
							firstKeptEntryId: event.preparation.firstKeptEntryId,
							tokensBefore: event.preparation.tokensBefore,
						},
					}));
				},
			});
			let nextRequest = "";
			faux.setResponses([
				call("update_plan", { steps: [{ id: "one", goal: "work", status: "in_progress" }] }),
				fauxAssistantMessage(
					[
						{ type: "text", text: "history".repeat(3000) },
						fauxToolCall("update_plan", {
							steps: [
								{ id: "one", goal: "work", status: "completed" },
								{ id: "two", goal: "report", status: "pending" },
							],
						}),
					],
					{ stopReason: "toolUse" },
				),
				(context) => {
					nextRequest = textOf(context);
					return fauxAssistantMessage("done");
				},
			]);
			await session.prompt("Work with a plan", { expandPromptTemplates: false });
			const count = manager.getEntries().filter((e) => e.type === "compaction").length;
			const nativeCount = (
				session as unknown as { _onlineCompact: { stateSnapshot: { nativeCompactionCount: number } } }
			)._onlineCompact.stateSnapshot.nativeCompactionCount;

			expect(count).toBe(1);
			if (check === "next-request") expect(nextRequest.includes(marker)).toBe(true);
			else expect(nativeCount).toBe(1);
		});
	}
});

it("runs an in-memory session without requiring an archive directory", async () => {
	const harness = await createHarness();
	try {
		harness.setResponses([fauxAssistantMessage("memory session completed")]);
		await harness.session.prompt("hello", { expandPromptTemplates: false });
		expect(harness.session.getLastAssistantText()).toBe("memory session completed");
		expect(harness.session.getActiveToolNames()).not.toContain("obs_recall");
		expect(harness.sessionManager.getEntries().filter((entry) => entry.type === "custom")).toHaveLength(0);
	} finally {
		harness.cleanup();
	}
});
