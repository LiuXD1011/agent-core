import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AssistantMessage } from "@liuxuedeng/agent-core-ai";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { exportSessionToJsonl } from "../src/app/session-export.ts";
import { exportSessionHtmlFromManager } from "../src/session/export/jsonl.ts";
import { loadEntriesFromFile, SessionManager } from "../src/session/store.ts";

const reply: AssistantMessage = {
	role: "assistant",
	provider: "faux",
	model: "offline",
	api: "openai-completions",
	content: [
		{ type: "text", text: "Completed" },
		{ type: "toolCall", id: "call", name: "bash", arguments: { command: "false" } },
	],
	usage: {
		input: 12,
		output: 4,
		cacheRead: 2,
		cacheWrite: 0,
		totalTokens: 18,
		cost: { input: 0.12, output: 0.04, cacheRead: 0.01, cacheWrite: 0, total: 0.17 },
	},
	stopReason: "toolUse",
	timestamp: 2,
};

describe("session export retains user-visible records", () => {
	let directory: string;
	let session: SessionManager;
	beforeEach(() => {
		directory = mkdtempSync(join(tmpdir(), "agent-export-retention-"));
		session = SessionManager.create(directory, directory);
		session.appendMessage({ role: "user", content: "Run a check", timestamp: 1 });
		session.appendMessage(reply);
		session.appendMessage({
			role: "toolResult",
			toolCallId: "call",
			toolName: "bash",
			content: [{ type: "text", text: "Command exited with code 1" }],
			isError: true,
			timestamp: 3,
		});
	});
	afterEach(() => rmSync(directory, { recursive: true, force: true }));
	it("exports JSONL with messages, tool errors, token counts and costs", () => {
		const output = exportSessionToJsonl(session, join(directory, "export.jsonl"));
		const records = loadEntriesFromFile(output);
		expect(records.slice(1)).toEqual(session.getBranch());
		expect(records).toContainEqual(expect.objectContaining({ type: "message", message: reply }));
		expect(records).toContainEqual(
			expect.objectContaining({
				type: "message",
				message: expect.objectContaining({ role: "toolResult", isError: true }),
			}),
		);
		expect(SessionManager.open(output, directory).buildSessionContext().messages).toEqual(
			session.buildSessionContext().messages,
		);
	});
	it("exports HTML with the same session records and usage data", async () => {
		// Exercises the application export operation directly: no terminal, theme,
		// extension, or model initialization.
		const output = await exportSessionHtmlFromManager(session, {
			outputPath: join(directory, "export.html"),
			themeName: "dark",
		});
		const html = readFileSync(output, "utf8");
		const match = html.match(/<script id="session-data" type="application\/json">([^<]+)<\/script>/);
		expect(match).not.toBeNull();
		const data: unknown = JSON.parse(Buffer.from(match![1], "base64").toString("utf8"));
		expect(data).toMatchObject({ entries: session.getEntries(), leafId: session.getLeafId() });
		expect(html).toContain("msg.usage.cost");
	});
});
