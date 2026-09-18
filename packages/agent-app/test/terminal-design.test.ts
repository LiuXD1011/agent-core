import { CURSOR_MARKER, setKeybindings, TuiMainScreen, visibleWidth } from "@liuxuedeng/agent-core-tui";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { defaultEditorTheme } from "../../tui/test/test-themes.ts";
import { VirtualTerminal } from "../../tui/test/virtual-terminal.ts";
import { KeybindingsManager } from "../src/ui/keybindings.ts";
import { CustomEditor } from "../src/ui/terminal/components/custom-editor.ts";
import { TerminalHeader } from "../src/ui/terminal/components/terminal-header.ts";
import { UserMessageComponent } from "../src/ui/terminal/components/user-message.ts";
import { initTheme } from "../src/ui/terminal/theme/theme.ts";
import { stripAnsi } from "../src/utils/ansi.ts";

beforeEach(() => {
	initTheme("dark");
	setKeybindings(new KeybindingsManager());
});

describe("compact terminal design", () => {
	it("uses five block-character rows at normal width and a text fallback on narrow screens", () => {
		const header = new TerminalHeader(() => "v0.1.0 · ~/project · main", "完整快捷键帮助");
		const lines = header.render(80);
		expect(lines.slice(0, 5).every((line) => stripAnsi(line).includes("█"))).toBe(true);
		expect(lines.slice(0, 5).every((line) => /^[ █]+$/.test(stripAnsi(line)))).toBe(true);
		expect(stripAnsi(header.render(40).join("\n"))).toContain("AGENT-CORE");
		expect(stripAnsi(lines.join("\n"))).not.toContain("完整快捷键帮助");
		for (const width of [8, 40, 58, 59, 80, 120]) {
			for (const line of header.render(width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
		}
		expect(header.render(0)).toEqual([]);
	});

	it("refreshes metadata and expands help without duplicating the wordmark", () => {
		let branch = "main";
		const header = new TerminalHeader(() => branch, "完整快捷键帮助");
		branch = "feature";
		header.setExpanded(true);
		const lines = header.render(80);
		expect(stripAnsi(lines.join("\n"))).toContain("feature");
		expect(stripAnsi(lines.join("\n"))).toContain("完整快捷键帮助");
		expect(lines.filter((line) => stripAnsi(line).includes("█"))).toHaveLength(5);
		header.setExpanded(false);
		expect(stripAnsi(header.render(80).join("\n"))).not.toContain("完整快捷键帮助");
	});

	it("keeps the placeholder out of input and submits the exact Chinese text", () => {
		const keys = new KeybindingsManager();
		const editor = new CustomEditor(new TuiMainScreen(new VirtualTerminal()), defaultEditorTheme, keys, {
			showInputHints: true,
		});
		editor.focused = true;
		const empty = editor.render(80).join("\n");
		expect(stripAnsi(empty)).toContain("输入消息，/ 查看命令");
		expect(empty.split(CURSOR_MARKER)).toHaveLength(2);
		expect(editor.getText()).toBe("");
		editor.handleInput("检查中文输入");
		expect(stripAnsi(editor.render(80).join("\n"))).not.toContain("输入消息");
		const submitted = vi.fn();
		editor.onSubmit = submitted;
		editor.handleInput("\r");
		expect(submitted).toHaveBeenCalledWith("检查中文输入");
	});

	it("handles remapped submission without adding a separate shortcut row", () => {
		const keys = new KeybindingsManager({
			"tui.input.submit": "ctrl+r",
			"app.interrupt": "ctrl+q",
			"app.clear": "ctrl+k",
		});
		setKeybindings(keys);
		const editor = new CustomEditor(new TuiMainScreen(new VirtualTerminal()), defaultEditorTheme, keys, {
			showInputHints: true,
		});
		expect(editor.render(120)).toHaveLength(3);
		editor.handleInput("中文");
		const submitted = vi.fn();
		editor.onSubmit = submitted;
		editor.handleInput("\x12");
		expect(submitted).toHaveBeenCalledWith("中文");
	});

	it("does not add hints to extension editors unless requested", () => {
		const editor = new CustomEditor(
			new TuiMainScreen(new VirtualTerminal()),
			defaultEditorTheme,
			new KeybindingsManager(),
		);
		expect(stripAnsi(editor.render(80).join("\n"))).not.toContain("输入消息");
		expect(editor.render(80)).toHaveLength(3);
	});

	it("keeps empty and multiline CJK input within the terminal width", () => {
		const editor = new CustomEditor(
			new TuiMainScreen(new VirtualTerminal()),
			defaultEditorTheme,
			new KeybindingsManager(),
			{ showInputHints: true },
		);
		editor.focused = true;
		for (const text of ["", `${"中文".repeat(50)}\n第二行`]) {
			editor.setText(text);
			for (const width of [8, 40, 80, 120]) {
				for (const line of editor.render(width)) expect(visibleWidth(line)).toBeLessThanOrEqual(width);
			}
		}
		expect(editor.render(0)).toEqual([]);
	});

	it("omits user labels without changing message text or shell integration zones", () => {
		const message = new UserMessageComponent("你说：助手的回复 /export");
		const output = message.render(80).join("\n");
		expect(
			stripAnsi(output)
				.split("\n")
				.map((line) => line.trim())
				.filter(Boolean),
		).toEqual(["你说：助手的回复 /export"]);
		expect(output.split("\x1b]133;A\x07")).toHaveLength(2);
		expect(output.split("\x1b]133;C\x07")).toHaveLength(2);
	});
});
