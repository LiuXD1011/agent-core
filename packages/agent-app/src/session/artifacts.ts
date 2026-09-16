/** Portable session evidence. Export is complete or fails explicitly. */
import { createHash, randomUUID } from "node:crypto";
import { existsSync, lstatSync, mkdirSync, readdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, sep } from "node:path";
import type { AgentMessage } from "@liuxuedeng/agent-core-agent";
import type { SessionManager } from "./store.ts";

export const ARCHIVE_TAIL_CUSTOM_TYPE = "export:context-archives";
export const ARCHIVE_TAIL_SCHEMA = "agent-session-archives/1";
export const MAX_FILE_BYTES = 4 * 1024 * 1024;
export const MAX_TOTAL_BYTES = 16 * 1024 * 1024;
const MAX_FILES = 4096;
const SKIP_NAMES = new Set(["ledger.jsonl"]);

export function contextRuntimeRoot(sessionManager: SessionManager): string | undefined {
	const sessionDir = sessionManager.getSessionDir();
	const sessionId = sessionManager.getSessionId();
	if (!sessionDir || !/^[a-z0-9][a-z0-9._-]*$/iu.test(sessionId)) return undefined;
	return join(sessionDir, "context", sessionId);
}

export function collectContextArchives(sessionManager: SessionManager): Record<string, string> | undefined {
	const files = Object.fromEntries(
		[...importedArchiveBytes(sessionManager)].map(([name, bytes]) => [name, bytes.toString("base64")]),
	);
	const root = contextRuntimeRoot(sessionManager);
	if (!root || !existsSync(root)) return Object.keys(files).length ? files : undefined;
	for (const path of [dirname(root), root]) {
		if (lstatSync(path).isSymbolicLink()) throw new Error("Session archive root must not be a symbolic link");
	}
	let total = Object.values(files).reduce((sum, value) => sum + Buffer.from(value, "base64").length, 0);
	const walk = (dir: string): void => {
		for (const entry of readdirSync(dir, { withFileTypes: true }).sort((a, b) => a.name.localeCompare(b.name))) {
			const full = join(dir, entry.name);
			if (entry.isSymbolicLink()) throw new Error(`Session archive contains a symbolic link: ${full}`);
			if (entry.isDirectory()) {
				walk(full);
				continue;
			}
			if (!entry.isFile() || SKIP_NAMES.has(entry.name)) continue;
			const name = relative(root, full).split(sep).join("/");
			if (files[name] !== undefined) {
				if (readFileSync(full).toString("base64") !== files[name])
					throw new Error(`Session archive conflict: ${name}`);
				continue;
			}
			const size = lstatSync(full).size;
			if (size > MAX_FILE_BYTES || total + size > MAX_TOTAL_BYTES || Object.keys(files).length >= MAX_FILES) {
				throw new Error(`Session archive exceeds export size limits: ${full}. No incomplete export was written.`);
			}
			const bytes = readFileSync(full);
			if (bytes.length !== size) throw new Error(`Session archive changed during export: ${full}`);
			files[relative(root, full).split(sep).join("/")] = bytes.toString("base64");
			total += size;
		}
	};
	walk(root);
	return Object.keys(files).length ? files : undefined;
}

export function buildArchiveTailEntry(sessionManager: SessionManager, parentId: string | null, timestamp: string) {
	const files = collectContextArchives(sessionManager);
	if (!files) return undefined;
	return {
		type: "custom" as const,
		customType: ARCHIVE_TAIL_CUSTOM_TYPE,
		id: `exp${randomUUID()}`,
		parentId,
		timestamp,
		data: {
			schema: ARCHIVE_TAIL_SCHEMA,
			sourceRoot: contextRuntimeRoot(sessionManager),
			files,
			checksums: Object.fromEntries(
				Object.entries(files).map(([name, data]) => [
					name,
					createHash("sha256").update(Buffer.from(data, "base64")).digest("hex"),
				]),
			),
		},
	};
}

function record(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

/** Validate carried evidence without writing files (also used by offline HTML export). */
function importedArchiveBytes(sessionManager: SessionManager): Map<string, Buffer> {
	const pending = new Map<string, Buffer>();
	let total = 0;
	for (const entry of sessionManager.getEntries()) {
		if (entry.type !== "custom" || entry.customType !== ARCHIVE_TAIL_CUSTOM_TYPE) continue;
		const data: unknown = entry.data;
		if (!record(data) || data.schema !== ARCHIVE_TAIL_SCHEMA || !record(data.files) || !record(data.checksums)) {
			throw new Error("Invalid session archive schema");
		}
		for (const [rel, content] of Object.entries(data.files)) {
			if (
				typeof content !== "string" ||
				isAbsolute(rel) ||
				rel.includes("\\") ||
				rel.includes("\0") ||
				rel.split("/").some((part) => !part || part === "." || part === "..") ||
				content.length > Math.ceil(MAX_FILE_BYTES / 3) * 4
			) {
				throw new Error(`Invalid session archive path or size: ${rel}`);
			}
			const bytes = Buffer.from(content, "base64");
			if (
				bytes.toString("base64") !== content ||
				bytes.length > MAX_FILE_BYTES ||
				createHash("sha256").update(bytes).digest("hex") !== data.checksums[rel]
			) {
				throw new Error(`Session archive checksum or encoding mismatch: ${rel}`);
			}
			const previous = pending.get(rel);
			if (previous && !previous.equals(bytes)) throw new Error(`Conflicting session archive: ${rel}`);
			if (!previous) {
				total += bytes.length;
				pending.set(rel, bytes);
			}
			if (total > MAX_TOTAL_BYTES || pending.size > MAX_FILES)
				throw new Error("Session archive exceeds import size limits");
		}
	}
	return pending;
}

/** Validate every payload before writing any file; never follow imported paths through symlinks. */
export function restoreContextArchives(sessionManager: SessionManager): number {
	const pending = importedArchiveBytes(sessionManager);
	const root = contextRuntimeRoot(sessionManager);
	if (!root) return 0;

	for (const [rel, bytes] of pending) {
		const target = join(root, ...rel.split("/"));
		const relativeTarget = relative(sessionManager.getSessionDir(), target);
		let cursor = sessionManager.getSessionDir();
		for (const part of relativeTarget.split(sep)) {
			cursor = join(cursor, part);
			if (lstatSync(cursor, { throwIfNoEntry: false })?.isSymbolicLink())
				throw new Error(`Session archive path is a symbolic link: ${cursor}`);
		}
		if (existsSync(target) && !readFileSync(target).equals(bytes))
			throw new Error(`Session archive conflicts with an existing file: ${rel}`);
	}
	for (const [rel, bytes] of pending) {
		const target = join(root, ...rel.split("/"));
		if (existsSync(target)) continue;
		mkdirSync(dirname(target), { recursive: true, mode: 0o700 });
		writeFileSync(target, bytes, { flag: "wx", mode: 0o600 });
	}
	return pending.size;
}

/** Relocate only validated receipt references in the model view; stored history remains unchanged. */
export function projectArchiveReferences(messages: AgentMessage[], sessionManager: SessionManager): AgentMessage[] {
	const root = contextRuntimeRoot(sessionManager);
	if (!root) return messages;
	const replacements = new Map<string, string>();
	for (const entry of sessionManager.getEntries()) {
		if (entry.type !== "custom" || entry.customType !== ARCHIVE_TAIL_CUSTOM_TYPE) continue;
		const data: unknown = entry.data;
		if (!record(data) || typeof data.sourceRoot !== "string" || !record(data.files)) continue;
		for (const name of Object.keys(data.files)) {
			const target = join(root, ...name.split("/"));
			if (existsSync(target)) replacements.set(`${data.sourceRoot}/${name}`, target);
		}
	}
	if (!replacements.size) return messages;
	return messages.map((message) => {
		if (message.role !== "toolResult") return message;
		return {
			...message,
			content: message.content.map((block) => {
				if (block.type !== "text" || !block.text.includes("context_evidence_receipt_v1")) return block;
				return {
					...block,
					text: block.text.replace(/^source_artifact=(.+)$/gm, (line, path: string) => {
						const target = replacements.get(path.replaceAll("\\", "/"));
						return target ? `source_artifact=${target}` : line;
					}),
				};
			}),
		};
	});
}
