import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import {
	exportSessionHtmlFromFile,
	exportSessionHtmlFromManager,
	exportSessionJsonlFromManager,
} from "../../src/session/export/jsonl.ts";
import { loadEntriesFromFile, SessionManager } from "../../src/session/store.ts";

function decodeSessionData(html: string): { header: unknown; entries: unknown[]; leafId: string | null } {
	const match = html.match(/<script id="session-data" type="application\/json">([^<]+)<\/script>/);
	expect(match).not.toBeNull();
	return JSON.parse(Buffer.from(match![1], "base64").toString("utf8"));
}

describe("application session export operations", () => {
	let directory: string;

	beforeEach(() => {
		directory = mkdtempSync(join(tmpdir(), "agent-app-export-"));
	});

	afterEach(() => rmSync(directory, { recursive: true, force: true }));

	function createBranchedSession(): { session: SessionManager; sessionFile: string } {
		const session = SessionManager.create(directory, directory);
		session.appendMessage({ role: "user", content: "First question", timestamp: 1 });
		const firstQuestionId = session.getLeafId()!;
		session.appendMessage({
			role: "assistant",
			provider: "faux",
			model: "offline",
			api: "openai-completions",
			content: [{ type: "text", text: "First answer" }],
			usage: {
				input: 5,
				output: 2,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 7,
				cost: { input: 0.05, output: 0.02, cacheRead: 0, cacheWrite: 0, total: 0.07 },
			},
			stopReason: "stop",
			timestamp: 2,
		});
		// Branch back to the first question and continue on a second branch; the
		// first answer is abandoned on a sibling branch.
		session.branch(firstQuestionId);
		session.appendMessage({ role: "user", content: "Second question", timestamp: 3 });
		session.appendMessage({
			role: "assistant",
			provider: "faux",
			model: "offline",
			api: "openai-completions",
			content: [{ type: "text", text: "Second answer" }],
			usage: {
				input: 5,
				output: 2,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 7,
				cost: { input: 0.05, output: 0.02, cacheRead: 0, cacheWrite: 0, total: 0.07 },
			},
			stopReason: "stop",
			timestamp: 4,
		});
		return { session, sessionFile: session.getSessionFile()! };
	}

	it("exports HTML from a session file without terminal, extension, or model initialization", async () => {
		const { sessionFile } = createBranchedSession();

		const output = await exportSessionHtmlFromFile(sessionFile, {
			themeName: "dark",
			outputPath: join(directory, "export.html"),
		});

		expect(existsSync(output)).toBe(true);
		const html = readFileSync(output, "utf8");
		const data = decodeSessionData(html);
		expect(data.entries).toHaveLength(4);
		expect(data.header).toMatchObject({ type: "session", cwd: directory });
		// Full-tree export includes entries abandoned by the branch.
		const texts = JSON.stringify(data.entries);
		expect(texts).toContain("First question");
		expect(texts).toContain("Second answer");
	});

	it("applies the requested theme to the exported page", async () => {
		const { sessionFile } = createBranchedSession();
		const dark = await exportSessionHtmlFromFile(sessionFile, {
			outputPath: join(directory, "dark.html"),
			themeName: "dark",
		});
		const light = await exportSessionHtmlFromFile(sessionFile, {
			outputPath: join(directory, "light.html"),
			themeName: "light",
		});
		const darkHtml = readFileSync(dark, "utf8");
		const lightHtml = readFileSync(light, "utf8");
		expect(darkHtml).not.toBe(lightHtml);
		expect(darkHtml).toContain("--exportPageBg");
		expect(lightHtml).toContain("--exportPageBg");
	});

	it("exports JSONL for the current branch only and re-imports to the same context", () => {
		const { session } = createBranchedSession();

		const output = exportSessionJsonlFromManager(session, join(directory, "branch.jsonl"));

		const records = loadEntriesFromFile(output);
		// Header + the 3 entries on the active branch; the abandoned first answer is excluded.
		expect(records).toHaveLength(4);
		expect(records[0]).toMatchObject({ type: "session" });
		expect(SessionManager.open(output, directory).buildSessionContext().messages).toEqual(
			session.buildSessionContext().messages,
		);
		const reimportedTexts = JSON.stringify(loadEntriesFromFile(output));
		expect(reimportedTexts).not.toContain("First answer");
		expect(reimportedTexts).toContain("Second answer");
	});

	it("rejects exporting an in-memory session and a missing file", async () => {
		const inMemory = SessionManager.inMemory(directory);
		await expect(exportSessionHtmlFromManager(inMemory)).rejects.toThrow("in-memory session");
		await expect(exportSessionHtmlFromFile(join(directory, "missing.jsonl"))).rejects.toThrow("File not found");
	});
});
