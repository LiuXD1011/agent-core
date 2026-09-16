import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { createObsRecallToolDefinition } from "../../src/context/observations/index.ts";
import type { ExtensionContext } from "../../src/extensions/types.ts";
import {
	ARCHIVE_TAIL_CUSTOM_TYPE,
	collectContextArchives,
	contextRuntimeRoot,
	projectArchiveReferences,
	restoreContextArchives,
} from "../../src/session/artifacts.ts";
import { exportSessionHtmlFromFile, exportSessionJsonlFromManager } from "../../src/session/export/jsonl.ts";
import { buildContextEntries, loadEntriesFromFile, SessionManager } from "../../src/session/store.ts";

const OBSERVATION_BODY = "archived large tool output\n".repeat(200);
const OBSERVATION_ID = `obs_${"a".repeat(24)}`;
const REDUCER_BODY = "ERROR something failed\nat row 1\n".repeat(120);

function seedArchives(sessionManager: SessionManager): void {
	const root = contextRuntimeRoot(sessionManager);
	if (!root) throw new Error("expected a persisted session with a runtime root");
	mkdirSync(join(root, "observation-pack", "objects"), { recursive: true, mode: 0o700 });
	writeFileSync(join(root, "observation-pack", "objects", `${OBSERVATION_ID}.txt`), OBSERVATION_BODY, { mode: 0o600 });
	mkdirSync(join(root, "evidence-preserving-reducer", "objects"), { recursive: true, mode: 0o700 });
	writeFileSync(join(root, "evidence-preserving-reducer", "objects", "deadbeef.log"), REDUCER_BODY, { mode: 0o600 });
}

describe("portable context archives in JSONL export", () => {
	let sourceDirectory: string;
	let targetDirectory: string;

	beforeEach(() => {
		sourceDirectory = mkdtempSync(join(tmpdir(), "agent-archive-src-"));
		targetDirectory = mkdtempSync(join(tmpdir(), "agent-archive-dst-"));
	});

	afterEach(() => {
		rmSync(sourceDirectory, { recursive: true, force: true });
		rmSync(targetDirectory, { recursive: true, force: true });
	});

	function createSourceSession(): SessionManager {
		const session = SessionManager.create(sourceDirectory, sourceDirectory);
		session.appendMessage({ role: "user", content: "run the build", timestamp: 1 });
		session.appendMessage({
			role: "assistant",
			provider: "faux",
			model: "offline",
			api: "openai-completions",
			content: [{ type: "text", text: "build failed, see log" }],
			usage: {
				input: 5,
				output: 3,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 8,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "stop",
			timestamp: 2,
		});
		return session;
	}

	it("carries archives in a trailing entry and restores them on import", async () => {
		const source = createSourceSession();
		seedArchives(source);

		const exportedPath = exportSessionJsonlFromManager(source, join(sourceDirectory, "export.jsonl"), {
			includeArchives: true,
		});

		// The trailing entry carries both payload files.
		const records = loadEntriesFromFile(exportedPath);
		const tail = records.find(
			(record) =>
				record.type === "custom" && (record as { customType?: string }).customType === ARCHIVE_TAIL_CUSTOM_TYPE,
		) as { data: { files: Record<string, string> } } | undefined;
		expect(tail).toBeDefined();
		const files = Object.keys(tail!.data.files);
		expect(files.some((file) => file.endsWith(`${OBSERVATION_ID}.txt`))).toBe(true);
		expect(files.some((file) => file.endsWith("deadbeef.log"))).toBe(true);
		expect(Buffer.from(tail!.data.files[files[0]]!, "base64").length).toBeGreaterThan(0);

		// "Move to another machine": import the file from a different directory.
		const importedPath = join(targetDirectory, "imported.jsonl");
		writeFileSync(importedPath, readFileSync(exportedPath));
		const imported = SessionManager.open(importedPath, targetDirectory, targetDirectory);
		const restored = restoreContextArchives(imported);

		expect(restored).toBe(2);
		const newRoot = contextRuntimeRoot(imported);
		expect(newRoot).toBeTruthy();
		expect(newRoot && !newRoot.startsWith(sourceDirectory)).toBe(true);
		expect(readFileSync(join(newRoot!, "observation-pack", "objects", `${OBSERVATION_ID}.txt`), "utf8")).toBe(
			OBSERVATION_BODY,
		);
		expect(readFileSync(join(newRoot!, "evidence-preserving-reducer", "objects", "deadbeef.log"), "utf8")).toBe(
			REDUCER_BODY,
		);

		// The carried archives never enter the model context.
		const contextTexts = buildContextEntries(imported.getEntries(), imported.getLeafId())
			.map((entry) => (entry.type === "message" ? JSON.stringify(entry.message) : JSON.stringify(entry)))
			.join("\n");
		expect(contextTexts).not.toContain("archived large tool output");
	});

	it("restored archives keep obs_recall working on the imported session", async () => {
		const source = createSourceSession();
		seedArchives(source);
		const exportedPath = exportSessionJsonlFromManager(source, join(sourceDirectory, "export.jsonl"), {
			includeArchives: true,
		});
		const importedPath = join(targetDirectory, "imported.jsonl");
		writeFileSync(importedPath, readFileSync(exportedPath));
		const imported = SessionManager.open(importedPath, targetDirectory, targetDirectory);
		restoreContextArchives(imported);

		const tool = createObsRecallToolDefinition({
			getRuntimeRoot: () => contextRuntimeRoot(imported)!,
			isRecallAvailable: () => true,
			showSavings: () => {},
		});
		const noContext = undefined as unknown as ExtensionContext;
		const result = await tool.execute(
			"call-recall",
			{ id: OBSERVATION_ID, offset: 0 },
			undefined,
			undefined,
			noContext,
		);
		const text = result.content.flatMap((block) => (block.type === "text" ? [block.text] : [])).join("");
		expect(text).toContain("obs_recall");
		expect(text).toContain(OBSERVATION_BODY.trim().split("\n")[0]!);
		await expect(
			tool.execute("call-recall-bad", { id: `obs_${"f".repeat(24)}` }, undefined, undefined, noContext),
		).rejects.toThrow("Unknown observation id");
	});

	it("exports without archives when the flag is off or nothing is archived", () => {
		const source = createSourceSession();
		const plain = exportSessionJsonlFromManager(source, join(sourceDirectory, "plain.jsonl"));
		expect(loadEntriesFromFile(plain).some((record) => (record as { customType?: string }).customType)).toBe(false);

		seedArchives(source);
		const withoutFlag = exportSessionJsonlFromManager(source, join(sourceDirectory, "no-flag.jsonl"), {
			includeArchives: false,
		});
		expect(loadEntriesFromFile(withoutFlag).some((record) => (record as { customType?: string }).customType)).toBe(
			false,
		);
		expect(existsSync(join(sourceDirectory, "no-flag.jsonl"))).toBe(true);
	});
	it("exports imported evidence to standalone HTML without the original archive directory", async () => {
		const source = createSourceSession();
		seedArchives(source);
		const exported = exportSessionJsonlFromManager(source, join(targetDirectory, "carried.jsonl"));
		rmSync(contextRuntimeRoot(source)!, { recursive: true });
		const imported = SessionManager.open(exported, targetDirectory, targetDirectory);
		expect(Object.keys(collectContextArchives(imported)!)).toHaveLength(2);
		const html = await exportSessionHtmlFromFile(exported, { outputPath: join(targetDirectory, "carried.html") });
		expect(readFileSync(html, "utf8")).toContain(Buffer.from(REDUCER_BODY).toString("base64"));
	});

	it("relocates diagnostic receipt paths only in the model view", () => {
		const source = createSourceSession();
		seedArchives(source);
		const oldPath = join(contextRuntimeRoot(source)!, "evidence-preserving-reducer", "objects", "deadbeef.log");
		source.appendMessage({
			role: "toolResult",
			toolCallId: "call-1",
			toolName: "bash",
			isError: true,
			timestamp: 3,
			content: [{ type: "text", text: `context_evidence_receipt_v1\nsource_artifact=${oldPath}` }],
		});
		const exported = exportSessionJsonlFromManager(source, join(targetDirectory, "carried.jsonl"));
		const imported = SessionManager.open(exported, targetDirectory, targetDirectory);
		restoreContextArchives(imported);
		const messages = imported.buildSessionContext().messages;
		const projected = JSON.stringify(projectArchiveReferences(messages, imported));
		expect(projected).toContain(
			join(contextRuntimeRoot(imported)!, "evidence-preserving-reducer", "objects", "deadbeef.log"),
		);
		expect(JSON.stringify(messages)).toContain(oldPath);
	});
});
