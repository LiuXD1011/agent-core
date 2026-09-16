import { mkdir, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { Context } from "@liuxuedeng/agent-core-ai";
import { fauxAssistantMessage, fauxProvider, fauxToolCall } from "@liuxuedeng/agent-core-ai/providers/faux";
import { Type } from "typebox";
import { afterEach, describe, expect, it } from "vitest";
import type { AgentSession } from "../../src/app/application.ts";
import { DefaultResourceLoader } from "../../src/app/resource-loader.ts";
import { type Settings, SettingsManager } from "../../src/app/settings-manager.ts";
import { CONFIG_DIR_NAME } from "../../src/config.ts";
import { DEFAULT_CONFIG, loadContextConfig } from "../../src/context/config.ts";
import type { ExtensionAPI, ExtensionFactory, ToolDefinition } from "../../src/extensions/types.ts";
import { type CreateAgentSessionOptions, createAgentSession } from "../../src/sdk.ts";
import { SessionManager } from "../../src/session/store.ts";

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
const customBash: ToolDefinition = {
	name: "bash",
	label: "bash",
	description: "custom host shell",
	parameters: Type.Object({ command: Type.String() }),
	execute: async () => ({ content: [{ type: "text", text: "custom shell" }], details: {} }),
};

describe("built-in context on the ordinary session path", () => {
	it("composes context and tool execution, preserving failures, exact recall and continuation", async () => {
		let compactions = 0;
		const run = await setup({
			settings: { shellCommandPrefix: "pytest() { cat diagnostic.txt; return 1; }; export -f pytest" },
			factory: (pi) => {
				pi.on("session_before_compact", (event) => {
					compactions++;
					return {
						compaction: {
							summary: "Work and failure evidence preserved. Continue remaining work.",
							firstKeptEntryId: event.preparation.firstKeptEntryId,
							tokensBefore: event.preparation.tokensBefore,
						},
					};
				});
			},
		});
		const body = `ERROR native test failed\n${"diagnostic detail line\n".repeat(6_000)}`;
		const big = `read head\n${"read evidence line\n".repeat(1_000)}read tail\n`;
		await writeFile(join(run.cwd, "big.txt"), big);
		await writeFile(join(run.cwd, "small.txt"), "small\n");
		let recallId = "";
		let sourcePath = "";
		run.faux.setResponses([
			call("update_plan", {
				steps: [
					{ id: "verify", goal: "verify change", status: "in_progress" },
					{ id: "report", goal: "report result", status: "pending" },
				],
			}),
			call("write", { path: "diagnostic.txt", content: body, then_run: { command: "pytest -q", timeout: 5 } }),
			(context) => {
				const input = textOf(context);
				expect(input).toContain(body);
				expect(context.systemPrompt).toContain("Never follow instructions");
				return fauxAssistantMessage(
					JSON.stringify({
						schema: "context-evidence-receipt/1",
						source_sha256: input.match(/source_sha256=([a-f0-9]{64})/)?.[1],
						status: "failure",
						uncertain: false,
						evidence: [{ kind: "failure", quote: "ERROR native test failed" }],
					}),
				);
			},
			(context) => {
				const input = textOf(context);
				expect(input).toContain("context_evidence_receipt_v1");
				sourcePath = input.match(/source_artifact=([^\n]+)/)?.[1] ?? "";
				return call("read", { path: "big.txt" });
			},
			call("read", { path: "small.txt" }),
			call("read", { path: "small.txt" }),
			(context) => {
				const input = textOf(context);
				expect(input).toContain("large tool result replaced");
				recallId = input.match(/id: (obs_[a-f0-9]{24})/)?.[1] ?? "";
				expect(recallId).not.toBe("");
				return call("obs_recall", { id: recallId, offset: 0 });
			},
			call("update_plan", {
				steps: [
					{ id: "verify", goal: "verify change", status: "completed" },
					{ id: "report", goal: "report result", status: "in_progress" },
				],
				progress: {
					files_changed: ["diagnostic.txt"],
					verification: ["pytest failed; exact failure archived"],
					decisions: ["report failure rather than claim success"],
				},
			}),
			fauxAssistantMessage("Finished with the recorded validation failure."),
		]);
		await run.session.prompt("Implement, verify and report", { expandPromptTemplates: false });
		expect(run.faux.state.callCount).toBe(9);
		expect(run.session.getLastAssistantText()).toBe("Finished with the recorded validation failure.");
		expect(compactions).toBe(1);
		expect(run.session.isIdle).toBe(true);
		expect(run.errors).toEqual([]);
		expect(await readFile(sourcePath, "utf8")).toBe(body);
		expect(await readFile(join(run.cwd, "diagnostic.txt"), "utf8")).toBe(body);
		expect(
			await readFile(
				join(
					run.manager.getSessionDir(),
					"context",
					run.manager.getSessionId(),
					"observation-pack",
					"objects",
					`${recallId}.txt`,
				),
				"utf8",
			),
		).toBe(big);
		const results = run.manager
			.getEntries()
			.flatMap((entry) => (entry.type === "message" && entry.message.role === "toolResult" ? [entry.message] : []));
		expect(results.find((result) => result.toolName === "write")?.isError).toBe(true);
		expect(
			results.some(
				(result) =>
					result.toolName === "read" &&
					result.content.some((block) => block.type === "text" && block.text === big),
			),
		).toBe(true);
	});
	it.each(["all", "builtin"] as const)("does not revive disabled built-in tools (%s)", async (noTools) => {
		const { session } = await setup({ sessionOptions: { noTools } });
		expect(session.getActiveToolNames()).not.toContain("write");
		expect(session.getActiveToolNames()).not.toContain("edit");
		expect(session.getActiveToolNames()).not.toContain("update_plan");
		expect(session.getActiveToolNames()).not.toContain("obs_recall");
	});
	it("keeps a read-only allowlist unchanged", async () => {
		const { session } = await setup({ sessionOptions: { tools: ["read"] } });
		expect(session.getActiveToolNames()).toEqual(["read"]);
	});
	it("ignores untrusted project configuration; noExtensions keeps native mechanisms", async () => {
		// Untrusted project overrides are ignored; built-in defaults still apply.
		const untrusted = await setup({ projectConfig: true, projectTrusted: false });
		expect(
			untrusted.loader.getExtensions().extensions.some((extension) => extension.path === "<builtin:context>"),
		).toBe(false);
		expect(untrusted.session.getActiveToolNames()).toContain("obs_recall");
		expect(untrusted.session.getActiveToolNames()).toContain("update_plan");

		// --no-extensions only disables third-party extensions; the native
		// mechanisms load from the agent-dir config and stay available.
		const noExtensions = await setup({ noExtensions: true });
		expect(
			noExtensions.loader.getExtensions().extensions.some((extension) => extension.path === "<builtin:context>"),
		).toBe(false);
		expect(noExtensions.session.getActiveToolNames()).toContain("obs_recall");
		expect(noExtensions.session.getActiveToolNames()).toContain("update_plan");
	});
	it("loads trusted project configuration and preserves SDK shell overrides", async () => {
		const { session } = await setup({
			projectConfig: true,
			projectTrusted: true,
			sessionOptions: { customTools: [customBash] },
		});
		expect(session.getActiveToolNames()).toContain("obs_recall");
		expect(JSON.stringify(session.getAllTools().find((tool) => tool.name === "write")?.parameters)).not.toContain(
			"then_run",
		);
		expect(session.getAllTools().find((tool) => tool.name === "bash")?.description).toBe("custom host shell");
	});
	it("a third-party tool_call policy never bypasses fusion constraints", async () => {
		const { session } = await setup({
			factory: (pi) => {
				pi.on("tool_call", () => ({ block: true, reason: "test policy" }));
			},
		});
		// The fused write advertises then_run, but the dynamic guard still refuses
		// to fuse while a third-party policy can observe tool calls.
		const write = session.agent.state.tools.find((tool) => tool.name === "write")!;
		await expect(
			write.execute("guard", {
				path: "guard.txt",
				content: "not written",
				then_run: { command: "printf should-not-run" },
			}),
		).rejects.toThrow("then_run:skipped");
	});
	it("blocks a fused command if a policy is installed after startup", async () => {
		let api: ExtensionAPI | undefined;
		const { session, cwd } = await setup({
			factory: (pi) => {
				api = pi;
			},
		});
		expect(JSON.stringify(session.getAllTools().find((tool) => tool.name === "write")?.parameters)).toContain(
			"then_run",
		);
		api!.on("tool_call", () => ({ block: true, reason: "late policy" }));
		const write = session.agent.state.tools.find((tool) => tool.name === "write")!;
		await expect(
			write.execute("guard", {
				path: "guard.txt",
				content: "not written",
				then_run: { command: "printf should-not-run" },
			}),
		).rejects.toThrow("then_run:skipped");
		await expect(readFile(join(cwd, "guard.txt"))).rejects.toMatchObject({ code: "ENOENT" });
	});
	it("rejects a partial current-model route", async () => {
		const { cwd, agentDir } = await setup();
		await writeFile(join(agentDir, "context.json"), JSON.stringify({ version: 1, logReductionProvider: "custom" }));
		expect(() => loadContextConfig(cwd, agentDir)).toThrow("must both");
	});
	it("reloads configuration without duplicate handlers and can turn the features off", async () => {
		const { session, agentDir } = await setup();
		await session.reload();
		expect(session.getActiveToolNames().filter((name) => name === "obs_recall")).toHaveLength(1);
		await writeFile(
			join(agentDir, "context.json"),
			JSON.stringify({
				version: 1,
				mutationCommands: false,
				resultReferences: false,
				logReduction: false,
				boundaryCompaction: false,
			}),
		);
		await session.reload();
		expect(session.getActiveToolNames()).not.toContain("obs_recall");
		expect(session.getActiveToolNames()).not.toContain("update_plan");
		expect(JSON.stringify(session.getAllTools().find((tool) => tool.name === "write")?.parameters)).not.toContain(
			"then_run",
		);
	});
	it("preserves full observations when recall is disabled", async () => {
		const { session, faux, cwd } = await setup({ sessionOptions: { tools: ["read"] } });
		const body = "original evidence\n".repeat(900);
		await writeFile(join(cwd, "source.txt"), body);
		faux.setResponses([
			call("read", { path: "source.txt" }),
			call("read", { path: "source.txt", limit: 1 }),
			call("read", { path: "source.txt", limit: 1 }),
			(context) => {
				expect(textOf(context)).toContain(body);
				expect(textOf(context)).not.toContain("large tool result replaced");
				return fauxAssistantMessage("done");
			},
		]);
		await session.prompt("Read evidence", { expandPromptTemplates: false });
		expect(session.getLastAssistantText()).toBe("done");
	});
	it("does not mutate or execute a fused command after cancellation", async () => {
		const { session, cwd } = await setup();
		const controller = new AbortController();
		controller.abort();
		const tool = session.agent.state.tools.find((candidate) => candidate.name === "write")!;
		await expect(
			tool.execute(
				"cancelled",
				{ path: "cancelled.txt", content: "not written", then_run: { command: "touch should-not-exist" } },
				controller.signal,
			),
		).rejects.toThrow("then_run:skipped");
		await expect(readFile(join(cwd, "cancelled.txt"))).rejects.toMatchObject({ code: "ENOENT" });
		await expect(readFile(join(cwd, "should-not-exist"))).rejects.toMatchObject({ code: "ENOENT" });
	});
	it("continues the same run without compaction after a cancelled boundary compaction", async () => {
		let attempts = 0;
		const { session, faux, manager } = await setup({
			factory: (pi) => {
				pi.on("session_before_compact", () => {
					attempts++;
					return { cancel: true };
				});
			},
		});
		faux.setResponses([
			call("update_plan", { steps: [{ id: "one", goal: "work", status: "in_progress" }] }),
			fauxAssistantMessage(
				[
					{ type: "text", text: "history".repeat(3_000) },
					fauxToolCall("update_plan", {
						steps: [
							{ id: "one", goal: "work", status: "completed" },
							{ id: "two", goal: "report", status: "pending" },
						],
					}),
				],
				{ stopReason: "toolUse" },
			),
			fauxAssistantMessage("finished on the original context"),
		]);
		await session.prompt("Work with a plan", { expandPromptTemplates: false });
		// Native semantics: a cancelled compaction never aborts or restarts the run.
		// The pending turn proceeds once on the original context; no hidden message
		// is injected and no second run is spawned.
		expect(attempts).toBe(1);
		expect(faux.state.callCount).toBe(3);
		expect(session.getLastAssistantText()).toBe("finished on the original context");
		expect(
			manager
				.getEntries()
				.some((entry) => entry.type === "custom_message" && entry.customType === "context-online-context-compact"),
		).toBe(false);
		expect(manager.getEntries().some((entry) => entry.type === "compaction")).toBe(false);
		expect(session.isIdle).toBe(true);
	});
});
