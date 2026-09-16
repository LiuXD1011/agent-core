/**
 * Application-level session export operations.
 *
 * HTML (full session tree) and JSONL (current branch) exports read a session
 * snapshot only: no terminal initialization, extension loading, or model access.
 * TUI, RPC, SDK, and the CLI all route through these operations.
 */

import { existsSync } from "fs";
import { exportSessionToJsonl } from "../../app/session-export.ts";
import { getResolvedThemeColors, getThemeExportColors } from "../../app/theme-data.ts";
import { resolvePath } from "../../utils/paths.ts";
import { buildArchiveTailEntry, collectContextArchives } from "../artifacts.ts";
import { snapshotRecord } from "../snapshot.ts";
import { SessionManager } from "../store.ts";
import {
	assertExportableSessionFile,
	type ExportTheme,
	generateSessionHtml,
	preRenderCustomTools,
	type RenderedToolHtml,
	type SessionExportSnapshot,
	type ToolHtmlRenderer,
	writeSessionHtml,
} from "./html/index.ts";

/** Session-scoped presentation state included in HTML exports. */
export interface SessionExportState {
	systemPrompt?: string;
	tools?: ReadonlyArray<{ name: string; description: string; parameters: unknown }>;
}

export interface HtmlExportOptions {
	outputPath?: string;
	themeName?: string;
}

/** Resolve a theme name into plain color data for the HTML generator. */
export function resolveExportTheme(themeName?: string): ExportTheme {
	try {
		return { colors: getResolvedThemeColors(themeName), exportColors: getThemeExportColors(themeName) };
	} catch {
		return { colors: getResolvedThemeColors("dark"), exportColors: getThemeExportColors("dark") };
	}
}

function buildExportSnapshot(
	sessionManager: SessionManager,
	state?: SessionExportState,
	toolRenderer?: ToolHtmlRenderer,
): SessionExportSnapshot {
	const entries = snapshotRecord(sessionManager.getEntries());

	// Pre-render custom tools if a tool renderer is provided
	let renderedTools: Record<string, RenderedToolHtml> | undefined;
	if (toolRenderer) {
		const preRendered = preRenderCustomTools(entries, toolRenderer);
		// Only include if we actually rendered something
		if (Object.keys(preRendered).length > 0) {
			renderedTools = preRendered;
		}
	}

	return {
		header: snapshotRecord(sessionManager.getHeader()),
		archives: collectContextArchives(sessionManager),
		entries,
		leafId: sessionManager.getLeafId(),
		systemPrompt: state?.systemPrompt,
		tools: state?.tools?.map((tool) => ({
			name: tool.name,
			description: tool.description,
			parameters: tool.parameters,
		})),
		renderedTools,
	};
}

/**
 * Export the current session to a self-contained HTML page (full session tree).
 * Accepts an optional TUI tool renderer; custom tools fall back to structured
 * rendering when it is not provided.
 */
export async function exportSessionHtmlFromManager(
	sessionManager: SessionManager,
	options: HtmlExportOptions & { state?: SessionExportState; toolRenderer?: ToolHtmlRenderer } = {},
): Promise<string> {
	const sessionFile = assertExportableSessionFile(sessionManager.getSessionFile());
	const snapshot = buildExportSnapshot(sessionManager, options.state, options.toolRenderer);
	const html = generateSessionHtml(snapshot, resolveExportTheme(options.themeName));
	return writeSessionHtml(sessionFile, html, options.outputPath);
}

/**
 * Export any session file to HTML without an agent runtime: reads the file,
 * generates the page, and writes the result.
 */
export async function exportSessionHtmlFromFile(inputPath: string, options: HtmlExportOptions = {}): Promise<string> {
	const resolvedInputPath = resolvePath(inputPath);
	if (!existsSync(resolvedInputPath)) {
		throw new Error(`File not found: ${resolvedInputPath}`);
	}

	const sessionManager = SessionManager.open(resolvedInputPath);
	const html = generateSessionHtml(buildExportSnapshot(sessionManager), resolveExportTheme(options.themeName));
	return writeSessionHtml(resolvedInputPath, html, options.outputPath);
}

/**
 * Export the current session branch (header + entries on the active path) as
 * resumable JSONL. With `includeArchives`, context archive payloads travel
 * in a dedicated trailing entry so `obs_recall` keeps working after an import.
 */
export function exportSessionJsonlFromManager(
	sessionManager: SessionManager,
	outputPath?: string,
	options: { includeArchives?: boolean } = {},
): string {
	return exportSessionToJsonl(sessionManager, outputPath, (parentId, timestamp) => {
		if (options.includeArchives === false) return [];
		const entry = buildArchiveTailEntry(sessionManager, parentId, timestamp);
		return entry ? [entry] : [];
	});
}
