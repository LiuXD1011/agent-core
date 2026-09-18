import { Container, SettingsList, setKeybindings, visibleWidth } from "@liuxuedeng/agent-core-tui";
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import tpsExtension from "../../../.agent-core/extensions/tps.ts";
import type { AgentSession } from "../src/app/application.ts";
import type { ReadonlyFooterDataProvider } from "../src/app/footer-data-provider.ts";
import type { ExtensionAPI } from "../src/extensions/types.ts";
import { KeybindingsManager } from "../src/ui/keybindings.ts";
import { FooterComponent } from "../src/ui/terminal/components/footer.ts";
import { LoginCancelledError } from "../src/ui/terminal/components/login-dialog.ts";
import { ThinkingSelectorComponent } from "../src/ui/terminal/components/thinking-selector.ts";
import { InteractiveMode } from "../src/ui/terminal/interactive-mode.ts";
import { getMarkdownTheme, initTheme } from "../src/ui/terminal/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";

const usage = { input: 100, output: 20, cacheRead: 300, cacheWrite: 0, totalTokens: 420, cost: { total: 0.01 } };

beforeAll(() => initTheme("dark"));
beforeEach(() => setKeybindings(new KeybindingsManager()));
afterEach(() => vi.restoreAllMocks());

describe("Chinese display boundaries", () => {
	it("labels usage and reasoning without changing stored usage or enum values", () => {
		const entries = [{ type: "message", message: { role: "assistant", usage } }];
		const state = {
			model: { id: "deepseek-flash", provider: "deepseek", reasoning: true, contextWindow: 1_000_000 },
			thinkingLevel: "high",
		};
		const session = {
			state,
			sessionManager: { getEntries: () => entries, getCwd: () => "/tmp/中文", getSessionName: () => undefined },
			getContextUsage: () => ({ contextWindow: 1_000_000, percent: 0.4 }),
			modelRuntime: { isUsingSubscription: () => false },
		} as unknown as AgentSession;
		const footerData: ReadonlyFooterDataProvider = {
			getGitBranch: () => "main",
			getExtensionStatuses: () => new Map(),
			getAvailableProviderCount: () => 1,
			onBranchChange: () => () => {},
		};
		const before = JSON.stringify({ state, entries });
		const footer = new FooterComponent(session, footerData);
		const output = stripAnsi(footer.render(200).join("\n"));
		for (const text of [
			"输入 100",
			"输出 20",
			"缓存读 300",
			"缓存命中 75.0%",
			"上下文 0.4%",
			"推理强度：高",
			"deepseek-flash",
		])
			expect(output).toContain(text);
		expect(output).not.toMatch(/CH75|思考：|推理强度：high/);
		for (const width of [40, 80, 120])
			for (const line of footer.render(width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		expect(JSON.stringify({ state, entries })).toBe(before);
	});

	it("searches Chinese reasoning labels and returns the original API enum", () => {
		const onSelect = vi.fn();
		const selector = new ThinkingSelectorComponent(
			"medium",
			["off", "minimal", "low", "medium", "high", "xhigh", "max"],
			onSelect,
			vi.fn(),
			vi.fn(),
			"medium",
		);
		const text = stripAnsi(selector.render(120).join("\n"));
		expect(text).toContain("推理强度");
		expect(text).toContain("默认");
		expect(text).not.toMatch(/约 \d+k|cycles thinking|· default/);
		selector.handleInput("超高");
		expect(selector.getSelectList().getSelectedItem()?.value).toBe("xhigh");
		selector.handleInput("\r");
		expect(onSelect).toHaveBeenCalledWith("xhigh");
	});

	it("keeps English reasoning command values searchable", () => {
		const select = vi.fn();
		const selector = new ThinkingSelectorComponent("medium", ["medium", "high"], select, vi.fn());
		selector.handleInput("high");
		selector.handleInput("\r");
		expect(select).toHaveBeenCalledWith("high");
	});

	it("renders project TPS statistics in Chinese without mutating messages", () => {
		const notify = vi.fn();
		const messages = [{ role: "assistant", usage }];
		const before = JSON.stringify(messages);
		type Event = { messages: typeof messages };
		type Context = { hasUI: boolean; ui: { notify: typeof notify } };
		const handlers = new Map<string, (event: Event, ctx: Context) => void>();
		tpsExtension({
			on: (name: string, handler: (event: Event, ctx: Context) => void) => handlers.set(name, handler),
		} as unknown as ExtensionAPI);
		const time = vi.spyOn(Date, "now").mockReturnValue(1000);
		const ctx = { hasUI: true, ui: { notify } };
		handlers.get("agent_start")?.({ messages: [] }, ctx);
		time.mockReturnValue(2000);
		handlers.get("agent_end")?.({ messages }, ctx);
		expect(notify).toHaveBeenCalledWith("耗时 1.0 秒 · 平均输出 20.0 Token/秒", "info");
		expect(JSON.stringify(messages)).toBe(before);
		handlers.get("agent_end")?.({ messages }, ctx);
		expect(notify).toHaveBeenCalledTimes(1);
	});

	it("renders Chinese hotkey help using configured bindings", () => {
		const context = {
			chatContainer: new Container(),
			ui: { requestRender: vi.fn() },
			session: { extensionRunner: { getShortcuts: () => new Map() } },
			keybindings: new KeybindingsManager(),
			getAppKeyDisplay: () => "Ctrl+R",
			getEditorKeyDisplay: () => "Ctrl+J",
			getMarkdownThemeWithSettings: () => getMarkdownTheme(),
		};
		const handle = Reflect.get(InteractiveMode.prototype, "handleHotkeysCommand") as (this: typeof context) => void;
		handle.call(context);
		const output = stripAnsi(context.chatContainer.render(120).join("\n"));
		for (const text of ["导航", "编辑", "切换推理强度", "Ctrl+R", "斜杠命令"]) expect(output).toContain(text);
		expect(output).not.toMatch(/Navigation|Action|Cycle thinking/);
	});

	it("exports JSONL with the unchanged path and a Chinese success notice", async () => {
		const context = {
			getPathCommandArgument: () => "/tmp/中文会话.jsonl",
			session: { exportToJsonl: vi.fn(() => "/tmp/中文会话.jsonl") },
			showStatus: vi.fn(),
			showError: vi.fn(),
		};
		const handle = Reflect.get(InteractiveMode.prototype, "handleExportCommand") as (
			this: typeof context,
			text: string,
		) => Promise<void>;
		await handle.call(context, "/export /tmp/中文会话.jsonl");
		expect(context.session.exportToJsonl).toHaveBeenCalledWith("/tmp/中文会话.jsonl");
		expect(context.showStatus).toHaveBeenCalledWith("会话已导出至：/tmp/中文会话.jsonl");
		expect(context.showError).not.toHaveBeenCalled();
	});
});

describe("language-independent control flow", () => {
	it("formats settings without changing the value returned by a key binding", () => {
		setKeybindings(new KeybindingsManager({ "tui.select.confirm": "ctrl+r", "tui.select.cancel": "ctrl+q" }));
		const change = vi.fn();
		const identity = (text: string) => text;
		const list = new SettingsList(
			[
				{
					id: "enabled",
					label: "自动压缩",
					currentValue: "true",
					values: ["true", "false"],
					formatValue: (value) => (value === "true" ? "开启" : "关闭"),
				},
			],
			5,
			{ label: identity, value: identity, description: identity, cursor: "→ ", hint: identity },
			change,
			vi.fn(),
		);
		expect(list.render(80).join("\n")).toContain("开启");
		expect(list.render(80).join("\n")).toContain("ctrl+r");
		list.handleInput("\x12");
		expect(change).toHaveBeenCalledWith("enabled", "false");
		expect(list.render(80).join("\n")).toContain("关闭");
		for (const width of [8, 40, 80, 120])
			for (const line of list.render(width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
	});

	it.each([false, true])("classifies login cancellation by identity, not translated text (%s)", async (cancelled) => {
		const error = cancelled ? new LoginCancelledError() : new Error("已取消登录");
		if (cancelled) error.message = "Different display text";
		const context = {
			session: { model: undefined },
			editorContainer: new Container(),
			editor: new Container(),
			ui: { setFocus: vi.fn(), requestRender: vi.fn() },
			loginProvider: vi.fn().mockRejectedValue(error),
			showError: vi.fn(),
		};
		const handle = Reflect.get(InteractiveMode.prototype, "showApiKeyLoginDialog") as (
			this: typeof context,
			id: string,
			name: string,
		) => Promise<void>;
		await handle.call(context, "faux", "Faux");
		if (cancelled) expect(context.showError).not.toHaveBeenCalled();
		else expect(context.showError).toHaveBeenCalledWith("保存 Faux 的 API 密钥失败：已取消登录");
	});
});
