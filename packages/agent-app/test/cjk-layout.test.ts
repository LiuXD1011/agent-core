import { visibleWidth } from "@liuxuedeng/agent-core-tui";
import { beforeAll, describe, expect, it, vi } from "vitest";
import type { AgentSession } from "../src/app/application.ts";
import type { ReadonlyFooterDataProvider } from "../src/app/footer-data-provider.ts";
import type { ModelRuntime } from "../src/app/model-runtime.ts";
import { listModels } from "../src/cli/list-models.ts";
import type { SessionTreeNode } from "../src/session/store.ts";
import { FooterComponent } from "../src/ui/terminal/components/footer.ts";
import { TreeSelectorComponent } from "../src/ui/terminal/components/tree-selector.ts";
import { initTheme } from "../src/ui/terminal/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";

function createSession(sessionName: string): AgentSession {
	const session = {
		state: {
			model: {
				id: "测试模型",
				provider: "测试服务商",
				contextWindow: 200_000,
				reasoning: true,
			},
			thinkingLevel: "high",
		},
		sessionManager: {
			getEntries: () => [],
			getSessionName: () => sessionName,
			getCwd: () => "/tmp/中文目录",
		},
		getContextUsage: () => ({ contextWindow: 200_000, percent: 42.5 }),
		modelRuntime: {
			isUsingSubscription: () => false,
		},
	};
	return session as unknown as AgentSession;
}

function createFooterData(): ReadonlyFooterDataProvider {
	const provider = {
		getGitBranch: () => "中文分支",
		getExtensionStatuses: () => new Map<string, string>(),
		getAvailableProviderCount: () => 1,
		onBranchChange: () => () => {},
	};
	return provider as unknown as ReadonlyFooterDataProvider;
}

describe("CJK terminal layout", () => {
	beforeAll(() => {
		initTheme(undefined, false);
	});

	for (const width of [40, 80, 120]) {
		it(`footer stays within ${width} columns with Chinese text`, () => {
			const footer = new FooterComponent(createSession("中文会话名称"), createFooterData());
			const lines = footer.render(width);
			expect(lines.length).toBeGreaterThan(0);
			for (const line of lines) {
				expect(visibleWidth(line), `line exceeds ${width}: ${stripAnsi(line)}`).toBeLessThanOrEqual(width);
			}
			const plain = lines.map((line) => stripAnsi(line));
			// 内容断言只在较宽的终端下做；40 列时内容会被截断（宽度约束优先）。
			if (width >= 80) {
				expect(plain.join("\n")).toContain("中文会话名称");
				expect(plain.join("\n")).toContain("推理强度：高");
			}
		});
	}

	it("aligns list-models columns using display width, not string length", async () => {
		const modelRuntime = {
			getError: () => undefined,
			getAvailable: async () => [
				{
					provider: "openai",
					id: "gpt-test",
					name: "GPT Test",
					contextWindow: 200_000,
					maxTokens: 8_000,
					reasoning: true,
					input: ["text"],
				},
				{
					provider: "zai",
					id: "glm-test",
					name: "GLM Test",
					contextWindow: 1_000_000,
					maxTokens: 16_000,
					reasoning: false,
					input: ["text", "image"],
				},
			],
		} as unknown as ModelRuntime;
		const logs: string[] = [];
		const logSpy = vi.spyOn(console, "log").mockImplementation((...args: unknown[]) => {
			logs.push(args.map(String).join(" "));
		});
		try {
			await listModels(modelRuntime);
		} finally {
			logSpy.mockRestore();
		}
		const plain = logs.map((line) => stripAnsi(line));
		const header = plain.find((line) => line.includes("服务商"));
		expect(header).toBeDefined();
		// Header and data rows must align: identical visible width per line.
		const widths = plain.filter((line) => line.trim().length > 0).map((line) => visibleWidth(line));
		expect(new Set(widths).size).toBe(1);
	});

	it("tree help renders Chinese labels without exceeding narrow widths", () => {
		const entry = {
			type: "message",
			id: "u1",
			parentId: null,
			timestamp: "2026-01-01T00:00:00.000Z",
			message: { role: "user", content: "你好", timestamp: 1 },
		} as never;
		const leaf: SessionTreeNode = { entry, children: [] };
		const selector = new TreeSelectorComponent(
			[leaf],
			"u1",
			24,
			() => {},
			() => {},
		);
		const lines = selector.render(40).map(stripAnsi);
		for (const line of lines) {
			expect(visibleWidth(line)).toBeLessThanOrEqual(40);
		}
		expect(lines.join("\n")).toContain("分支");
	});
});
