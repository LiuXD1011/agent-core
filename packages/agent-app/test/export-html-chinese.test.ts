import { readFileSync } from "node:fs";
import { runInNewContext } from "node:vm";
import { describe, expect, it } from "vitest";
import { generateSessionHtml, type SessionExportSnapshot } from "../src/session/export/html/index.ts";
import { THINKING_LEVEL_LABELS } from "../src/ui/thinking-labels.ts";

const template = readFileSync(new URL("../src/session/export/html/template.js", import.meta.url), "utf8");
const names = [
	"formatThinkingLevel",
	"escapeHtml",
	"getTreeNodeDisplayHtml",
	"getSearchableText",
	"formatTokens",
	"renderHeader",
];
const functions = names
	.map((name) => {
		const source = template.match(new RegExp(`      function ${name}\\([^]*?\\n      }`))?.[0];
		if (!source) throw new Error(`Missing template function: ${name}`);
		return source;
	})
	.join("\n");

describe("Chinese HTML display preserves session data", () => {
	it.each(Object.entries(THINKING_LEVEL_LABELS))(
		"renders and searches %s without translating the stored enum",
		(level, label) => {
			const entry = { type: "thinking_level_change", thinkingLevel: level };
			const before = structuredClone(entry);
			const result = runInNewContext(
				`${functions}; ({ html: getTreeNodeDisplayHtml(entry), search: getSearchableText(entry) })`,
				{ entry },
			) as { html: string; search: string };
			expect(result.html).toContain(`[推理强度：${label}]`);
			expect(result.search).toContain(level);
			expect(result.search).toContain(label);
			expect(entry).toEqual(before);
		},
	);
	it.each(["<img src=x onerror=alert(1)>", "__proto__", "constructor"])(
		"escapes unknown enum %s without prototype lookup",
		(level) => {
			const html = runInNewContext(`${functions}; getTreeNodeDisplayHtml(entry)`, {
				entry: { type: "thinking_level_change", thinkingLevel: level },
			}) as string;
			expect(html).toContain(level.replaceAll("<", "&lt;").replaceAll(">", "&gt;"));
			expect(html).not.toContain("<img");
			expect(html).not.toContain("[object Object]");
		},
	);
	it("renders readable usage labels without changing their numbers", () => {
		const globalStats = {
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			tokens: { input: 230, output: 94, cacheRead: 3968, cacheWrite: 12 },
			models: [],
			userMessages: 1,
			assistantMessages: 1,
			toolResults: 0,
			customMessages: 0,
			compactions: 0,
			branchSummaries: 0,
			toolCalls: 0,
		};
		const html = runInNewContext(`${functions}; renderHeader()`, {
			globalStats,
			header: null,
			systemPrompt: "",
			tools: [],
		}) as string;
		expect(html).toContain("输入 230 输出 94 缓存读 4.0k 缓存写 12");
		expect(html).toContain("显示/隐藏推理");
		expect(globalStats.tokens.cacheRead).toBe(3968);
	});
	it("embeds the exact input snapshot rather than Chinese display values", () => {
		const snapshot: SessionExportSnapshot = {
			header: null,
			entries: [
				{
					type: "thinking_level_change",
					id: "level",
					parentId: null,
					timestamp: "2026-09-16T00:00:00.000Z",
					thinkingLevel: "high",
				},
			],
			leafId: "level",
		};
		const before = structuredClone(snapshot);
		const html = generateSessionHtml(snapshot, { colors: {}, exportColors: {} });
		const encoded = html.match(/<script[^>]*id="session-data"[^>]*>([^<]+)<\/script>/)?.[1];
		expect(encoded).toBeDefined();
		expect(JSON.parse(Buffer.from(encoded!, "base64").toString("utf8"))).toEqual(before);
		expect(snapshot).toEqual(before);
	});
});
