/**
 * Redraws Extension
 *
 * Exposes /tui to show TUI redraw stats.
 */

import type { ExtensionAPI } from "@liuxuedeng/agent-core";
import { Text } from "@liuxuedeng/agent-core-tui";

export default function (pi: ExtensionAPI) {
	pi.registerCommand("tui", {
		description: "显示终端界面统计",
		handler: async (_args, ctx) => {
			if (!ctx.hasUI) return;
			let redraws = 0;
			await ctx.ui.custom<void>((tui, _theme, _keybindings, done) => {
				redraws = tui.fullRedraws;
				done(undefined);
				return new Text("", 0, 0);
			});
			ctx.ui.notify(`终端界面完整重绘次数：${redraws}`, "info");
		},
	});
}
