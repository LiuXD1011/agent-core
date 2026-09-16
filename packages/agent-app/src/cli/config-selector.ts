/**
 * TUI config selector for `pi config` command
 */

import { ProcessTerminal, type TUI, TuiMainScreen } from "@liuxuedeng/agent-core-tui";
import type { SettingsManager } from "../app/settings-manager.ts";
import { ConfigSelectorComponent, type ScopedResolvedPaths } from "../ui/terminal/components/config-selector.ts";
import { initTheme, stopThemeWatcher } from "../ui/terminal/theme/theme.ts";

export interface ConfigSelectorOptions {
	resolvedPaths: ScopedResolvedPaths;
	settingsManager: SettingsManager;
	cwd: string;
	agentDir: string;
	writeScope: "global" | "project";
	projectModeAvailable: boolean;
}

/** Show TUI config selector and return when closed */
export async function selectConfig(options: ConfigSelectorOptions): Promise<void> {
	// Initialize theme before showing TUI
	initTheme(options.settingsManager.getTheme(), true);

	return new Promise((resolve) => {
		const ui: TUI = new TuiMainScreen(
			new ProcessTerminal(),
			options.settingsManager.getShowHardwareCursor(),
			options.agentDir,
		);
		ui.setClearOnShrink(options.settingsManager.getClearOnShrink());
		let resolved = false;

		const selector = new ConfigSelectorComponent(
			options.resolvedPaths,
			options.settingsManager,
			options.cwd,
			options.agentDir,
			() => {
				if (!resolved) {
					resolved = true;
					ui.stop();
					stopThemeWatcher();
					resolve();
				}
			},
			() => {
				ui.stop();
				stopThemeWatcher();
				process.exit(0);
			},
			() => ui.requestRender(),
			ui.terminal.rows,
			options.writeScope,
			options.projectModeAvailable,
		);

		ui.addChild(selector);
		ui.setFocus(selector.getResourceList());
		ui.start();
	});
}
