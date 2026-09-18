import { existsSync, readFileSync, writeFileSync } from "fs";
import { basename, join } from "path";
import { APP_NAME, getExportTemplateDir } from "../../../config.ts";
import { normalizePath } from "../../../utils/paths.ts";
import type { SessionEntry, SessionHeader } from "../../store.ts";

/**
 * Interface for rendering custom tools to HTML.
 * Used by agent-session to pre-render extension tool output.
 */
export interface ToolHtmlRenderer {
	/** Render a tool call to HTML. Returns undefined if tool has no custom renderer. */
	renderCall(toolCallId: string, toolName: string, args: unknown): string | undefined;
	/** Render a tool result to collapsed/expanded or undefined if tool has no custom renderer. */
	renderResult(
		toolCallId: string,
		toolName: string,
		result: Array<{ type: string; text?: string; data?: string; mimeType?: string }>,
		details: unknown,
		isError: boolean,
	): { collapsed?: string; expanded?: string } | undefined;
}

/** Pre-rendered HTML for a custom tool call and result */
export interface RenderedToolHtml {
	callHtml?: string;
	resultHtmlCollapsed?: string;
	resultHtmlExpanded?: string;
}

/** Theme colors for HTML export, resolved by the caller (see app/session-export.ts). */
export interface ExportTheme {
	colors: Record<string, string>;
	exportColors: { pageBg?: string; cardBg?: string; infoBg?: string };
}

/** Read-only snapshot of a session used to generate an export. */
export interface SessionExportSnapshot {
	header: SessionHeader | null;
	/** Embedded evidence files, downloadable without the original machine. */
	archives?: Record<string, string>;
	entries: SessionEntry[];
	leafId: string | null;
	systemPrompt?: string;
	tools?: Array<{ name: string; description: string; parameters: unknown }>;
	/** Pre-rendered HTML for custom tool calls/results, keyed by tool call ID */
	renderedTools?: Record<string, RenderedToolHtml>;
}

/** Parse a color string to RGB values. Supports hex (#RRGGBB) and rgb(r,g,b) formats. */
function parseColor(color: string): { r: number; g: number; b: number } | undefined {
	const hexMatch = color.match(/^#([0-9a-fA-F]{2})([0-9a-fA-F]{2})([0-9a-fA-F]{2})$/);
	if (hexMatch) {
		return {
			r: Number.parseInt(hexMatch[1], 16),
			g: Number.parseInt(hexMatch[2], 16),
			b: Number.parseInt(hexMatch[3], 16),
		};
	}
	const rgbMatch = color.match(/^rgb\s*\(\s*(\d+)\s*,\s*(\d+)\s*,\s*(\d+)\s*\)$/);
	if (rgbMatch) {
		return {
			r: Number.parseInt(rgbMatch[1], 10),
			g: Number.parseInt(rgbMatch[2], 10),
			b: Number.parseInt(rgbMatch[3], 10),
		};
	}
	return undefined;
}

/** Calculate relative luminance of a color (0-1, higher = lighter). */
function getLuminance(r: number, g: number, b: number): number {
	const toLinear = (c: number) => {
		const s = c / 255;
		return s <= 0.03928 ? s / 12.92 : ((s + 0.055) / 1.055) ** 2.4;
	};
	return 0.2126 * toLinear(r) + 0.7152 * toLinear(g) + 0.0722 * toLinear(b);
}

/** Adjust color brightness. Factor > 1 lightens, < 1 darkens. */
function adjustBrightness(color: string, factor: number): string {
	const parsed = parseColor(color);
	if (!parsed) return color;
	const adjust = (c: number) => Math.min(255, Math.max(0, Math.round(c * factor)));
	return `rgb(${adjust(parsed.r)}, ${adjust(parsed.g)}, ${adjust(parsed.b)})`;
}

/** Derive export background colors from a base color (e.g., userMessageBg). */
function deriveExportColors(baseColor: string): { pageBg: string; cardBg: string; infoBg: string } {
	const parsed = parseColor(baseColor);
	if (!parsed) {
		return {
			pageBg: "rgb(24, 24, 30)",
			cardBg: "rgb(30, 30, 36)",
			infoBg: "rgb(60, 55, 40)",
		};
	}

	const luminance = getLuminance(parsed.r, parsed.g, parsed.b);
	const isLight = luminance > 0.5;

	if (isLight) {
		return {
			pageBg: adjustBrightness(baseColor, 0.96),
			cardBg: baseColor,
			infoBg: `rgb(${Math.min(255, parsed.r + 10)}, ${Math.min(255, parsed.g + 5)}, ${Math.max(0, parsed.b - 20)})`,
		};
	}
	return {
		pageBg: adjustBrightness(baseColor, 0.7),
		cardBg: adjustBrightness(baseColor, 0.85),
		infoBg: `rgb(${Math.min(255, parsed.r + 20)}, ${Math.min(255, parsed.g + 15)}, ${parsed.b})`,
	};
}

/**
 * Generate CSS custom property declarations from resolved theme colors.
 */
function generateThemeVars(theme: ExportTheme): string {
	const lines: string[] = [];
	for (const [key, value] of Object.entries(theme.colors)) {
		lines.push(`--${key}: ${value};`);
	}

	// Use explicit theme export colors if available, otherwise derive from userMessageBg
	const userMessageBg = theme.colors.userMessageBg || "#343541";
	const derivedColors = deriveExportColors(userMessageBg);

	lines.push(`--exportPageBg: ${theme.exportColors.pageBg ?? derivedColors.pageBg};`);
	lines.push(`--exportCardBg: ${theme.exportColors.cardBg ?? derivedColors.cardBg};`);
	lines.push(`--exportInfoBg: ${theme.exportColors.infoBg ?? derivedColors.infoBg};`);

	return lines.join("\n      ");
}

/**
 * Core HTML generation logic shared by all export entry points.
 * Pure function of the session snapshot and theme data: no terminal, extension,
 * or model initialization is required.
 */
export function generateSessionHtml(snapshot: SessionExportSnapshot, theme: ExportTheme): string {
	const templateDir = getExportTemplateDir();
	const template = readFileSync(join(templateDir, "template.html"), "utf-8");
	const templateCss = readFileSync(join(templateDir, "template.css"), "utf-8");
	const templateJs = readFileSync(join(templateDir, "template.js"), "utf-8");
	const markedJs = readFileSync(join(templateDir, "vendor", "marked.min.js"), "utf-8");
	const hljsJs = readFileSync(join(templateDir, "vendor", "highlight.min.js"), "utf-8");

	const themeVars = generateThemeVars(theme);
	const colors = theme.colors;
	const themeExport = theme.exportColors;
	const derivedExportColors = deriveExportColors(colors.userMessageBg || "#343541");
	const bodyBg = themeExport.pageBg ?? derivedExportColors.pageBg;
	const containerBg = themeExport.cardBg ?? derivedExportColors.cardBg;
	const infoBg = themeExport.infoBg ?? derivedExportColors.infoBg;

	// Base64 encode session data to avoid escaping issues
	const sessionDataBase64 = Buffer.from(JSON.stringify(snapshot)).toString("base64");

	// Build the CSS with theme variables injected
	const css = templateCss
		.replace("{{THEME_VARS}}", themeVars)
		.replace("{{BODY_BG}}", bodyBg)
		.replace("{{CONTAINER_BG}}", containerBg)
		.replace("{{INFO_BG}}", infoBg);

	const archiveLinks = Object.entries(snapshot.archives ?? {})
		.map(([path, data]) => {
			const name = path
				.replaceAll("&", "&amp;")
				.replaceAll("<", "&lt;")
				.replaceAll(">", "&gt;")
				.replaceAll('"', "&quot;")
				.replaceAll("'", "&#39;");
			const payload = Buffer.from(data, "base64").toString("base64");
			return `<li><a download="${name}" href="data:application/octet-stream;base64,${payload}">${name}</a></li>`;
		})
		.join("");
	const archives = archiveLinks
		? `<details id="session-artifacts"><summary>归档证据</summary><ul>${archiveLinks}</ul></details>`
		: "";
	return template
		.replace("{{ARCHIVES}}", archives)
		.replace("{{CSS}}", css)
		.replace("{{JS}}", templateJs)
		.replace("{{SESSION_DATA}}", sessionDataBase64)
		.replace("{{MARKED_JS}}", markedJs)
		.replace("{{HIGHLIGHT_JS}}", hljsJs);
}

/** Tools rendered directly by the HTML template (not pre-rendered via TUI→ANSI→HTML pipeline) */
const TEMPLATE_RENDERED_TOOLS = new Set(["bash", "read", "write", "edit", "ls"]);

/**
 * Pre-render custom tools to HTML using their TUI renderers.
 */
export function preRenderCustomTools(
	entries: SessionEntry[],
	toolRenderer: ToolHtmlRenderer,
): Record<string, RenderedToolHtml> {
	const renderedTools: Record<string, RenderedToolHtml> = {};

	for (const entry of entries) {
		if (entry.type !== "message") continue;
		const msg = entry.message;

		// Find tool calls in assistant messages
		if (msg.role === "assistant" && Array.isArray(msg.content)) {
			for (const block of msg.content) {
				if (block.type === "toolCall" && !TEMPLATE_RENDERED_TOOLS.has(block.name)) {
					const callHtml = toolRenderer.renderCall(block.id, block.name, block.arguments);
					if (callHtml) {
						renderedTools[block.id] = { callHtml };
					}
				}
			}
		}

		// Find tool results
		if (msg.role === "toolResult" && msg.toolCallId) {
			const toolName = msg.toolName || "";
			// Only render if we have a pre-rendered call OR it's not template-rendered
			const existing = renderedTools[msg.toolCallId];
			if (existing || !TEMPLATE_RENDERED_TOOLS.has(toolName)) {
				const rendered = toolRenderer.renderResult(
					msg.toolCallId,
					toolName,
					msg.content,
					msg.details,
					msg.isError || false,
				);
				if (rendered) {
					renderedTools[msg.toolCallId] = {
						...existing,
						resultHtmlCollapsed: rendered.collapsed,
						resultHtmlExpanded: rendered.expanded,
					};
				}
			}
		}
	}

	return renderedTools;
}

/**
 * Write generated export HTML to disk and return the output path.
 * Shared by the app-level export operations.
 */
export function writeSessionHtml(sessionFile: string, html: string, outputPath?: string): string {
	let resolvedOutput = outputPath ? normalizePath(outputPath) : undefined;
	if (!resolvedOutput) {
		const sessionBasename = basename(sessionFile, ".jsonl");
		resolvedOutput = `${APP_NAME}-session-${sessionBasename}.html`;
	}

	writeFileSync(resolvedOutput, html, "utf8");
	return resolvedOutput;
}

/** Check that a session file can be exported (used by the app-level export operations). */
export function assertExportableSessionFile(sessionFile: string | undefined): string {
	if (!sessionFile) {
		throw new Error("Cannot export in-memory session to HTML");
	}
	if (!existsSync(sessionFile)) {
		throw new Error("Nothing to export yet - start a conversation first");
	}
	return sessionFile;
}
