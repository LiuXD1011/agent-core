import { setKeybindings, TuiMainScreen } from "@liuxuedeng/agent-core-tui";
import { afterEach, describe, expect, it } from "vitest";
import { defaultEditorTheme } from "../../tui/test/test-themes.ts";
import { VirtualTerminal } from "../../tui/test/virtual-terminal.ts";
import { KeybindingsManager } from "../src/ui/keybindings.ts";
import { CustomEditor } from "../src/ui/terminal/components/custom-editor.ts";

afterEach(() => {
	setKeybindings(new KeybindingsManager());
});

describe("CustomEditor prompt history keybindings", () => {
	it("gives an explicit history binding precedence over the ctrl+p app action", () => {
		const keybindings = new KeybindingsManager({
			"tui.editor.historyPrevious": "ctrl+p",
			"tui.editor.historyNext": "ctrl+n",
		});
		setKeybindings(keybindings);
		const editor = new CustomEditor(new TuiMainScreen(new VirtualTerminal()), defaultEditorTheme, keybindings);
		let pathToggles = 0;
		editor.onAction("app.session.togglePath", () => {
			pathToggles++;
		});
		editor.addToHistory("previous prompt");
		editor.setText("draft");

		editor.handleInput("\x10"); // Ctrl+P
		expect(editor.getText()).toBe("previous prompt");
		expect(pathToggles).toBe(0);

		editor.handleInput("\x0e"); // Ctrl+N
		expect(editor.getText()).toBe("draft");
	});
});
