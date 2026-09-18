import { setKeybindings, visibleWidth } from "@liuxuedeng/agent-core-tui";
import { beforeAll, beforeEach, describe, expect, it } from "vitest";
import type { AgentSession } from "../src/app/application.ts";
import type { ReadonlyFooterDataProvider } from "../src/app/footer-data-provider.ts";
import { KeybindingsManager } from "../src/ui/keybindings.ts";
import { FooterComponent, formatCwdForFooter } from "../src/ui/terminal/components/footer.ts";
import { initTheme } from "../src/ui/terminal/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";

type AssistantUsage = {
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	cost: { total: number };
};

function createSession(options: {
	sessionName: string;
	modelId?: string;
	provider?: string;
	reasoning?: boolean;
	thinkingLevel?: string;
	usage?: AssistantUsage;
	branchUsage?: AssistantUsage;
	compactionUsage?: AssistantUsage;
	toolUsage?: AssistantUsage;
	usingSubscription?: boolean;
}): AgentSession {
	const usage = options.usage;
	const entries: Array<Record<string, unknown>> = [];

	if (usage !== undefined) {
		entries.push({
			type: "message",
			message: {
				role: "assistant",
				usage,
			},
		});
	}

	if (options.branchUsage !== undefined) {
		entries.push({
			type: "branch_summary",
			usage: options.branchUsage,
		});
	}

	if (options.compactionUsage !== undefined) {
		entries.push({
			type: "compaction",
			usage: options.compactionUsage,
		});
	}

	if (options.toolUsage !== undefined) {
		entries.push({
			type: "message",
			message: {
				role: "toolResult",
				usage: options.toolUsage,
			},
		});
	}

	const session = {
		state: {
			model: {
				id: options.modelId ?? "test-model",
				provider: options.provider ?? "test",
				contextWindow: 200_000,
				reasoning: options.reasoning ?? false,
			},
			thinkingLevel: options.thinkingLevel ?? "off",
		},
		sessionManager: {
			getEntries: () => entries,
			getSessionName: () => options.sessionName,
			getCwd: () => "/tmp/project",
		},
		getContextUsage: () => ({ contextWindow: 200_000, percent: 12.3 }),
		modelRuntime: {
			isUsingSubscription: () => options.usingSubscription ?? false,
		},
	};

	return session as unknown as AgentSession;
}

function createFooterData(providerCount: number): ReadonlyFooterDataProvider {
	const provider = {
		getGitBranch: () => "main",
		getExtensionStatuses: () => new Map<string, string>(),
		getAvailableProviderCount: () => providerCount,
		onBranchChange: (callback: () => void) => {
			void callback;
			return () => {};
		},
	};

	return provider;
}

describe("formatCwdForFooter", () => {
	it("does not abbreviate sibling paths that share the home prefix", () => {
		expect(formatCwdForFooter("/home/user2", "/home/user")).toBe("/home/user2");
	});

	it("abbreviates the home directory and descendants", () => {
		expect(formatCwdForFooter("/home/user", "/home/user")).toBe("~");
		expect(formatCwdForFooter("/home/user/project", "/home/user")).toBe("~/project");
	});
});

describe("FooterComponent width handling", () => {
	beforeEach(() => setKeybindings(new KeybindingsManager()));
	beforeAll(() => {
		initTheme(undefined, false);
	});

	it("merges path and shortcuts, then usage in the requested order, into two rows", () => {
		const footer = new FooterComponent(
			createSession({
				sessionName: "",
				modelId: "deepseek-flash",
				provider: "deepseek",
				reasoning: true,
				thinkingLevel: "high",
				usage: { input: 602, output: 318, cacheRead: 12000, cacheWrite: 0, cost: { total: 0.0001 } },
			}),
			createFooterData(1),
		);
		const lines = footer.render(180).map(stripAnsi);
		expect(lines).toHaveLength(2);
		expect(lines[0]).toContain("/tmp/project (main)  Enter 发送 · Escape 中止 · Ctrl+C 清空");
		expect(lines[0]).toContain("deepseek / deepseek-flash");
		expect(lines[1]).toContain(
			"输入 602 · 输出 318 · 缓存命中 95.2% · 缓存读 12k · $0.000 · 上下文 12.3%/200k（自动压缩）",
		);
		expect(lines[1]).toContain("推理强度：高");
		for (const width of [1, 8, 40, 80, 120, 160]) {
			const narrow = footer.render(width);
			expect(narrow).toHaveLength(2);
			for (const line of narrow) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
	});

	it("shows rebound shortcuts on the path row and omits unbound actions", () => {
		setKeybindings(
			new KeybindingsManager({ "tui.input.submit": "ctrl+r", "app.interrupt": "ctrl+q", "app.clear": [] }),
		);
		const footer = new FooterComponent(createSession({ sessionName: "" }), createFooterData(1));
		const first = stripAnsi(footer.render(160)[0]);
		expect(first).toContain("Ctrl+R 发送 · Ctrl+Q 中止");
		expect(first).not.toContain("Enter");
		expect(first).not.toContain("清空");
	});

	it("keeps all lines within width for wide session names", () => {
		const width = 93;
		const session = createSession({ sessionName: "한글".repeat(30) });
		const footer = new FooterComponent(session, createFooterData(1));

		const lines = footer.render(width);
		for (const line of lines) {
			expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
	});

	it("keeps stats line within width for wide model and provider names", () => {
		const width = 60;
		const session = createSession({
			sessionName: "",
			modelId: "模".repeat(30),
			provider: "공급자",
			reasoning: true,
			thinkingLevel: "high",
			usage: {
				input: 12_345,
				output: 6_789,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 1.234 },
			},
		});
		const footer = new FooterComponent(session, createFooterData(2));

		const lines = footer.render(width);
		for (const line of lines) {
			expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
	});

	it("includes summary and tool result usage in the total cost", () => {
		const session = createSession({
			sessionName: "",
			usage: {
				input: 100,
				output: 10,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 0.5 },
			},
			branchUsage: {
				input: 20,
				output: 5,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 0.25 },
			},
			compactionUsage: {
				input: 5,
				output: 2,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 0.125 },
			},
			toolUsage: {
				input: 15,
				output: 3,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 0.375 },
			},
		});
		const footer = new FooterComponent(session, createFooterData(1));

		const statsLine = stripAnsi(footer.render(120)[1]);
		expect(statsLine).toContain("$1.250");
	});

	it("shows the latest cache hit rate when cache usage is present", () => {
		const session = createSession({
			sessionName: "",
			usage: {
				input: 100,
				output: 10,
				cacheRead: 50,
				cacheWrite: 50,
				cost: { total: 0.001 },
			},
		});
		const footer = new FooterComponent(session, createFooterData(1));

		const statsLine = stripAnsi(footer.render(120)[1]);
		expect(statsLine).toContain("缓存命中 25.0%");
	});

	it("marks Kimi Coding costs as subscription estimates", () => {
		const session = createSession({
			sessionName: "",
			provider: "kimi-coding",
			usage: {
				input: 100,
				output: 10,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 1.234 },
			},
		});
		const footer = new FooterComponent(session, createFooterData(1));

		expect(stripAnsi(footer.render(120)[1])).toContain("$1.234（订阅）");
	});

	it("marks explicitly identified subscription auth", () => {
		const session = createSession({ sessionName: "", provider: "anthropic", usingSubscription: true });
		const footer = new FooterComponent(session, createFooterData(1));

		expect(stripAnsi(footer.render(120)[1])).toContain("$0.000（订阅）");
	});

	it("does not mark generic OAuth sign-in as a subscription", () => {
		const session = createSession({
			sessionName: "",
			provider: "openrouter",
			usage: {
				input: 100,
				output: 10,
				cacheRead: 0,
				cacheWrite: 0,
				cost: { total: 1.234 },
			},
		});
		const footer = new FooterComponent(session, createFooterData(1));
		const stats = stripAnsi(footer.render(120)[1]);

		expect(stats).toContain("$1.234");
		expect(stats).not.toContain("（订阅）");
	});
});
