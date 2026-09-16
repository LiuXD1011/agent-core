import * as fs from "node:fs";
import * as path from "node:path";
import type { ThinkingLevel } from "@liuxuedeng/agent-core-agent";
import {
	type EditorTheme,
	getCapabilities,
	type MarkdownTheme,
	type SelectListTheme,
	type SettingsListTheme,
} from "@liuxuedeng/agent-core-tui";
import chalk from "chalk";
import type { SourceInfo } from "../../../app/source-info.ts";
import {
	assertThemeNameIsValid,
	detectTerminalBackgroundFromEnv,
	getAvailableThemes,
	getAvailableThemesWithPaths,
	getCurrentThemeName,
	getDefaultTheme,
	getResolvedThemeColors,
	getThemeExportColors,
	getThemeForRgbColor,
	isLightTheme,
	loadThemeJson,
	parseAutoThemeSetting,
	parseThemeJsonContent,
	type RgbColor,
	resolveThemeColors,
	resolveThemeSetting,
	setCurrentThemeName,
	setRegisteredThemeSources,
	setThemeJsonValidator,
	type TerminalTheme,
	type TerminalThemeDetection,
	type TerminalThemeDetectionOptions,
	type ThemeBg,
	type ThemeColor,
	type ThemeInfo,
	type ThemeJson,
	type ThemeJsonValidator,
	type ThemeResource,
	withThemeColorFallbacks,
} from "../../../app/theme-data.ts";
import { getCustomThemesDir } from "../../../config.ts";
import { closeWatcher, watchWithErrorHandler } from "../../../utils/fs-watch.ts";
import { highlight, supportsLanguage } from "../../../utils/syntax-highlight.ts";

// ============================================================================
// Types & Schema
// ============================================================================

export type { ValidatedThemeJson as ThemeJson } from "../../../app/theme-json.ts";
export type { ThemeColor, ThemeBg, ThemeInfo, ThemeJsonValidator, TerminalTheme };
export { setThemeJsonValidator };

type ColorMode = "truecolor" | "256color";

// ============================================================================
// Color Utilities
// ============================================================================

function hexToRgb(hex: string): { r: number; g: number; b: number } {
	const cleaned = hex.replace("#", "");
	if (cleaned.length !== 6) {
		throw new Error(`Invalid hex color: ${hex}`);
	}
	const r = parseInt(cleaned.substring(0, 2), 16);
	const g = parseInt(cleaned.substring(2, 4), 16);
	const b = parseInt(cleaned.substring(4, 6), 16);
	if (Number.isNaN(r) || Number.isNaN(g) || Number.isNaN(b)) {
		throw new Error(`Invalid hex color: ${hex}`);
	}
	return { r, g, b };
}

// The 6x6x6 color cube channel values (indices 0-5)
const CUBE_VALUES = [0, 95, 135, 175, 215, 255];

// Grayscale ramp values (indices 232-255, 24 grays from 8 to 238)
const GRAY_VALUES = Array.from({ length: 24 }, (_, i) => 8 + i * 10);

function findClosestCubeIndex(value: number): number {
	let minDist = Infinity;
	let minIdx = 0;
	for (let i = 0; i < CUBE_VALUES.length; i++) {
		const dist = Math.abs(value - CUBE_VALUES[i]);
		if (dist < minDist) {
			minDist = dist;
			minIdx = i;
		}
	}
	return minIdx;
}

function findClosestGrayIndex(gray: number): number {
	let minDist = Infinity;
	let minIdx = 0;
	for (let i = 0; i < GRAY_VALUES.length; i++) {
		const dist = Math.abs(gray - GRAY_VALUES[i]);
		if (dist < minDist) {
			minDist = dist;
			minIdx = i;
		}
	}
	return minIdx;
}

function colorDistance(r1: number, g1: number, b1: number, r2: number, g2: number, b2: number): number {
	// Weighted Euclidean distance (human eye is more sensitive to green)
	const dr = r1 - r2;
	const dg = g1 - g2;
	const db = b1 - b2;
	return dr * dr * 0.299 + dg * dg * 0.587 + db * db * 0.114;
}

function rgbTo256(r: number, g: number, b: number): number {
	// Find closest color in the 6x6x6 cube
	const rIdx = findClosestCubeIndex(r);
	const gIdx = findClosestCubeIndex(g);
	const bIdx = findClosestCubeIndex(b);
	const cubeR = CUBE_VALUES[rIdx];
	const cubeG = CUBE_VALUES[gIdx];
	const cubeB = CUBE_VALUES[bIdx];
	const cubeIndex = 16 + 36 * rIdx + 6 * gIdx + bIdx;
	const cubeDist = colorDistance(r, g, b, cubeR, cubeG, cubeB);

	// Find closest grayscale
	const gray = Math.round(0.299 * r + 0.587 * g + 0.114 * b);
	const grayIdx = findClosestGrayIndex(gray);
	const grayValue = GRAY_VALUES[grayIdx];
	const grayIndex = 232 + grayIdx;
	const grayDist = colorDistance(r, g, b, grayValue, grayValue, grayValue);

	// Check if color has noticeable saturation (hue matters)
	// If max-min spread is significant, prefer cube to preserve tint
	const maxC = Math.max(r, g, b);
	const minC = Math.min(r, g, b);
	const spread = maxC - minC;

	// Only consider grayscale if color is nearly neutral (spread < 10)
	// AND grayscale is actually closer
	if (spread < 10 && grayDist < cubeDist) {
		return grayIndex;
	}

	return cubeIndex;
}

function hexTo256(hex: string): number {
	const { r, g, b } = hexToRgb(hex);
	return rgbTo256(r, g, b);
}

function fgAnsi(color: string | number, mode: ColorMode): string {
	if (color === "") return "\x1b[39m";
	if (typeof color === "number") return `\x1b[38;5;${color}m`;
	if (color.startsWith("#")) {
		if (mode === "truecolor") {
			const { r, g, b } = hexToRgb(color);
			return `\x1b[38;2;${r};${g};${b}m`;
		} else {
			const index = hexTo256(color);
			return `\x1b[38;5;${index}m`;
		}
	}
	throw new Error(`Invalid color value: ${color}`);
}

function bgAnsi(color: string | number, mode: ColorMode): string {
	if (color === "") return "\x1b[49m";
	if (typeof color === "number") return `\x1b[48;5;${color}m`;
	if (color.startsWith("#")) {
		if (mode === "truecolor") {
			const { r, g, b } = hexToRgb(color);
			return `\x1b[48;2;${r};${g};${b}m`;
		} else {
			const index = hexTo256(color);
			return `\x1b[48;5;${index}m`;
		}
	}
	throw new Error(`Invalid color value: ${color}`);
}

type OptionalThemeColor = "scrollbarTrack" | "scrollbarThumb" | "thinkingMax" | "searchMatchText";
type OptionalThemeBg = "searchMatchBg";

// ============================================================================
// Theme Class
// ============================================================================

export class Theme {
	readonly name?: string;
	readonly sourcePath?: string;
	sourceInfo?: SourceInfo;
	private fgColors: Map<ThemeColor, string>;
	private bgColors: Map<ThemeBg, string>;
	private mode: ColorMode;

	constructor(
		fgColors: Record<Exclude<ThemeColor, OptionalThemeColor>, string | number> &
			Partial<Record<OptionalThemeColor, string | number>>,
		bgColors: Record<Exclude<ThemeBg, OptionalThemeBg>, string | number> &
			Partial<Record<OptionalThemeBg, string | number>>,
		mode: ColorMode,
		options: { name?: string; sourcePath?: string; sourceInfo?: SourceInfo } = {},
	) {
		this.name = options.name;
		this.sourcePath = options.sourcePath;
		this.sourceInfo = options.sourceInfo;
		this.mode = mode;
		this.fgColors = new Map();
		const colors = {
			...fgColors,
			scrollbarTrack: fgColors.scrollbarTrack ?? fgColors.muted,
			scrollbarThumb: fgColors.scrollbarThumb ?? fgColors.text,
			thinkingMax: fgColors.thinkingMax ?? fgColors.thinkingXhigh,
			searchMatchText: fgColors.searchMatchText ?? fgColors.text,
		};
		for (const [key, value] of Object.entries(colors) as [ThemeColor, string | number][]) {
			this.fgColors.set(key, fgAnsi(value, mode));
		}
		this.bgColors = new Map();
		const backgrounds = {
			...bgColors,
			searchMatchBg: bgColors.searchMatchBg ?? bgColors.selectedBg,
		};
		for (const [key, value] of Object.entries(backgrounds) as [ThemeBg, string | number][]) {
			this.bgColors.set(key, bgAnsi(value, mode));
		}
	}

	fg(color: ThemeColor, text: string): string {
		const ansi = this.fgColors.get(color);
		if (!ansi) throw new Error(`Unknown theme color: ${color}`);
		return `${ansi}${text}\x1b[39m`; // Reset only foreground color
	}

	bg(color: ThemeBg, text: string): string {
		const ansi = this.bgColors.get(color);
		if (!ansi) throw new Error(`Unknown theme background color: ${color}`);
		return `${ansi}${text}\x1b[49m`; // Reset only background color
	}

	bold(text: string): string {
		return chalk.bold(text);
	}

	italic(text: string): string {
		return chalk.italic(text);
	}

	underline(text: string): string {
		return chalk.underline(text);
	}

	inverse(text: string): string {
		return chalk.inverse(text);
	}

	strikethrough(text: string): string {
		return chalk.strikethrough(text);
	}

	getFgAnsi(color: ThemeColor): string {
		const ansi = this.fgColors.get(color);
		if (!ansi) throw new Error(`Unknown theme color: ${color}`);
		return ansi;
	}

	getBgAnsi(color: ThemeBg): string {
		const ansi = this.bgColors.get(color);
		if (!ansi) throw new Error(`Unknown theme background color: ${color}`);
		return ansi;
	}

	getColorMode(): ColorMode {
		return this.mode;
	}

	getThinkingBorderColor(level: ThinkingLevel): (str: string) => string {
		// Map thinking levels to dedicated theme colors
		switch (level) {
			case "off":
				return (str: string) => this.fg("thinkingOff", str);
			case "minimal":
				return (str: string) => this.fg("thinkingMinimal", str);
			case "low":
				return (str: string) => this.fg("thinkingLow", str);
			case "medium":
				return (str: string) => this.fg("thinkingMedium", str);
			case "high":
				return (str: string) => this.fg("thinkingHigh", str);
			case "xhigh":
				return (str: string) => this.fg("thinkingXhigh", str);
			case "max":
				return (str: string) => this.fg("thinkingMax", str);
			default:
				return (str: string) => this.fg("thinkingOff", str);
		}
	}

	getBashModeBorderColor(): (str: string) => string {
		return (str: string) => this.fg("bashMode", str);
	}
}

// ============================================================================
// Theme Loading
// ============================================================================

function createTheme(themeJson: ThemeJson, mode?: ColorMode, sourcePath?: string): Theme {
	const colorMode = mode ?? (getCapabilities().trueColor ? "truecolor" : "256color");
	const resolvedColors = resolveThemeColors(withThemeColorFallbacks(themeJson.colors), themeJson.vars);
	const fgColors: Record<ThemeColor, string | number> = {} as Record<ThemeColor, string | number>;
	const bgColors: Record<ThemeBg, string | number> = {} as Record<ThemeBg, string | number>;
	const bgColorKeys: Set<string> = new Set([
		"selectedBg",
		"searchMatchBg",
		"userMessageBg",
		"customMessageBg",
		"toolPendingBg",
		"toolSuccessBg",
		"toolErrorBg",
	]);
	for (const [key, value] of Object.entries(resolvedColors)) {
		if (bgColorKeys.has(key)) {
			bgColors[key as ThemeBg] = value;
		} else {
			fgColors[key as ThemeColor] = value;
		}
	}
	return new Theme(fgColors, bgColors, colorMode, {
		name: themeJson.name,
		sourcePath,
	});
}

export function loadThemeFromPath(themePath: string, mode?: ColorMode): Theme {
	const content = fs.readFileSync(themePath, "utf-8");
	const themeJson = parseThemeJsonContent(themePath, content);
	return createTheme(themeJson, mode, themePath);
}

function loadTheme(name: string, mode?: ColorMode): Theme {
	const registeredTheme = registeredThemes.get(name);
	if (registeredTheme) {
		return registeredTheme;
	}
	const themeJson = loadThemeJson(name);
	return createTheme(themeJson, mode);
}

export function getThemeByName(name: string): Theme | undefined {
	try {
		return loadTheme(name);
	} catch {
		return undefined;
	}
}

// Re-exported theme data API. TUI code keeps importing from this module; HTML
// export and other non-TUI consumers import the same functions from
// `core/theme-data.ts` without crossing into the interactive mode.
export {
	getAvailableThemes,
	getAvailableThemesWithPaths,
	getDefaultTheme,
	getResolvedThemeColors,
	getThemeExportColors,
	isLightTheme,
	parseAutoThemeSetting,
	resolveThemeColors,
	resolveThemeSetting,
	withThemeColorFallbacks,
	type TerminalThemeDetection,
	type TerminalThemeDetectionOptions,
	detectTerminalBackgroundFromEnv,
	getThemeForRgbColor,
	type RgbColor,
};

// ============================================================================
// Terminal Detection
// ============================================================================

export interface TerminalBackgroundThemeDetector {
	queryTerminalBackgroundColor({ timeoutMs }: { timeoutMs: number }): Promise<RgbColor | undefined>;
}

export interface TerminalAutoThemeDetector extends TerminalBackgroundThemeDetector {
	queryTerminalColorScheme?({ timeoutMs }: { timeoutMs: number }): Promise<TerminalTheme | undefined>;
}

export interface TerminalBackgroundThemeDetectionOptions {
	ui: TerminalBackgroundThemeDetector;
	timeoutMs: number;
	env?: NodeJS.ProcessEnv;
}

export interface TerminalAutoThemeDetectionOptions {
	ui: TerminalAutoThemeDetector;
	timeoutMs: number;
	env?: NodeJS.ProcessEnv;
}

export async function detectTerminalBackgroundTheme({
	ui,
	timeoutMs,
	env,
}: TerminalBackgroundThemeDetectionOptions): Promise<TerminalThemeDetection> {
	try {
		const rgb = await ui.queryTerminalBackgroundColor({ timeoutMs });
		if (rgb) {
			return {
				theme: getThemeForRgbColor(rgb),
				source: "terminal background",
				detail: `OSC 11 background rgb(${rgb.r}, ${rgb.g}, ${rgb.b})`,
				confidence: "high",
			};
		}
	} catch {
		// Fall back to environment-based detection when the terminal query fails.
	}

	return detectTerminalBackgroundFromEnv({ env });
}

export async function detectTerminalThemeForAuto({
	ui,
	timeoutMs,
	env,
}: TerminalAutoThemeDetectionOptions): Promise<TerminalTheme> {
	let colorSchemePromise: Promise<TerminalTheme | undefined> | undefined;
	try {
		colorSchemePromise = ui.queryTerminalColorScheme?.({ timeoutMs });
	} catch {
		// Fall back to OSC 11 / COLORFGBG detection when starting the color-scheme query fails.
	}
	const backgroundThemePromise = detectTerminalBackgroundTheme({ ui, timeoutMs, env });

	try {
		const colorScheme = await colorSchemePromise;
		if (colorScheme) return colorScheme;
	} catch {
		// Fall back to the concurrently queried OSC 11 / COLORFGBG detection.
	}
	return (await backgroundThemePromise).theme;
}

// ============================================================================
// Global Theme Instance
// ============================================================================

// Use globalThis to share theme across module loaders (tsx + jiti in dev mode)
const THEME_KEY = Symbol.for("@liuxuedeng/agent-core:theme");
const THEME_KEY_OLD = Symbol.for("@mariozechner/pi-coding-agent:theme");

// Export theme as a getter that reads from globalThis
// This ensures all module instances (tsx, jiti) see the same theme
export const theme: Theme = new Proxy({} as Theme, {
	get(_target, prop) {
		const t = (globalThis as Record<symbol, Theme>)[THEME_KEY];
		if (!t) throw new Error("Theme not initialized. Call initTheme() first.");
		return (t as unknown as Record<string | symbol, unknown>)[prop];
	},
});

function setGlobalTheme(t: Theme): void {
	(globalThis as Record<symbol, Theme>)[THEME_KEY] = t;
	(globalThis as Record<symbol, Theme>)[THEME_KEY_OLD] = t;
}

let themeWatcher: fs.FSWatcher | undefined;
let themeReloadTimer: NodeJS.Timeout | undefined;
let onThemeChangeCallback: (() => void) | undefined;
const registeredThemes = new Map<string, Theme>();

export function setRegisteredThemes(themes: Array<Theme | ThemeResource>): void {
	registeredThemes.clear();
	for (const theme of themes) {
		if (theme.name) {
			assertThemeNameIsValid(theme.name);
			const instance =
				theme instanceof Theme
					? theme
					: theme.json
						? createTheme(theme.json, undefined, theme.sourcePath)
						: theme.sourcePath
							? loadThemeFromPath(theme.sourcePath)
							: undefined;
			if (!instance) throw new Error(`Theme resource has no colors: ${theme.name}`);
			instance.sourceInfo = theme.sourceInfo;
			registeredThemes.set(theme.name, instance);
		}
	}
	setRegisteredThemeSources(
		themes.filter((theme) => theme.name).map((theme) => ({ name: theme.name!, sourcePath: theme.sourcePath })),
	);
}

export function initTheme(themeName?: string, enableWatcher: boolean = false): void {
	const name = themeName ?? getDefaultTheme();
	setCurrentThemeName(name);
	try {
		setGlobalTheme(loadTheme(name));
		if (enableWatcher) {
			startThemeWatcher();
		}
	} catch (_error) {
		// Theme is invalid - fall back to dark theme silently
		setCurrentThemeName("dark");
		setGlobalTheme(loadTheme("dark"));
		// Don't start watcher for fallback theme
	}
}

export function setTheme(name: string, enableWatcher: boolean = false): { success: boolean; error?: string } {
	setCurrentThemeName(name);
	try {
		setGlobalTheme(loadTheme(name));
		if (enableWatcher) {
			startThemeWatcher();
		}
		if (onThemeChangeCallback) {
			onThemeChangeCallback();
		}
		return { success: true };
	} catch (error) {
		// Theme is invalid - fall back to dark theme
		setCurrentThemeName("dark");
		setGlobalTheme(loadTheme("dark"));
		// Don't start watcher for fallback theme
		return {
			success: false,
			error: error instanceof Error ? error.message : String(error),
		};
	}
}

export function setThemeInstance(themeInstance: Theme): void {
	setGlobalTheme(themeInstance);
	setCurrentThemeName("<in-memory>");
	stopThemeWatcher(); // Can't watch a direct instance
	if (onThemeChangeCallback) {
		onThemeChangeCallback();
	}
}

export function onThemeChange(callback: () => void): void {
	onThemeChangeCallback = callback;
}

function startThemeWatcher(): void {
	stopThemeWatcher();

	// Only watch if it's a custom theme (not built-in)
	const currentThemeName = getCurrentThemeName();
	if (!currentThemeName || currentThemeName === "dark" || currentThemeName === "light") {
		return;
	}

	const customThemesDir = getCustomThemesDir();
	const watchedThemeName = currentThemeName;
	const watchedFileName = `${watchedThemeName}.json`;
	const themeFile = path.join(customThemesDir, watchedFileName);

	// Only watch if the file exists
	if (!fs.existsSync(themeFile)) {
		return;
	}

	const scheduleReload = () => {
		if (themeReloadTimer) {
			clearTimeout(themeReloadTimer);
		}
		themeReloadTimer = setTimeout(() => {
			themeReloadTimer = undefined;

			// Ignore stale timers after switching themes or stopping the watcher
			if (getCurrentThemeName() !== watchedThemeName) {
				return;
			}

			// Keep the last successfully loaded theme active if the file is temporarily missing
			if (!fs.existsSync(themeFile)) {
				return;
			}

			try {
				// Reload the theme from disk and refresh the registry cache
				const reloadedTheme = loadThemeFromPath(themeFile);
				registeredThemes.set(watchedThemeName, reloadedTheme);
				setGlobalTheme(reloadedTheme);
				// Notify callback (to invalidate UI)
				if (onThemeChangeCallback) {
					onThemeChangeCallback();
				}
			} catch (_error) {
				// Ignore errors (file might be in invalid state while being edited)
			}
		}, 100);
	};

	themeWatcher =
		watchWithErrorHandler(
			customThemesDir,
			(_eventType, filename) => {
				if (getCurrentThemeName() !== watchedThemeName) {
					return;
				}
				if (!filename) {
					scheduleReload();
					return;
				}
				if (filename !== watchedFileName) {
					return;
				}
				scheduleReload();
			},
			() => {
				closeWatcher(themeWatcher);
				themeWatcher = undefined;
			},
		) ?? undefined;
}

export function stopThemeWatcher(): void {
	if (themeReloadTimer) {
		clearTimeout(themeReloadTimer);
		themeReloadTimer = undefined;
	}
	closeWatcher(themeWatcher);
	themeWatcher = undefined;
}

// ============================================================================
// TUI Helpers
// ============================================================================

type CliHighlightTheme = Record<string, (s: string) => string>;

let cachedHighlightThemeFor: Theme | undefined;
let cachedCliHighlightTheme: CliHighlightTheme | undefined;

function buildCliHighlightTheme(t: Theme): CliHighlightTheme {
	return {
		keyword: (s: string) => t.fg("syntaxKeyword", s),
		built_in: (s: string) => t.fg("syntaxType", s),
		literal: (s: string) => t.fg("syntaxNumber", s),
		number: (s: string) => t.fg("syntaxNumber", s),
		regexp: (s: string) => t.fg("syntaxString", s),
		string: (s: string) => t.fg("syntaxString", s),
		comment: (s: string) => t.fg("syntaxComment", s),
		doctag: (s: string) => t.fg("syntaxComment", s),
		meta: (s: string) => t.fg("muted", s),
		function: (s: string) => t.fg("syntaxFunction", s),
		title: (s: string) => t.fg("syntaxFunction", s),
		class: (s: string) => t.fg("syntaxType", s),
		type: (s: string) => t.fg("syntaxType", s),
		tag: (s: string) => t.fg("syntaxPunctuation", s),
		name: (s: string) => t.fg("syntaxKeyword", s),
		attr: (s: string) => t.fg("syntaxVariable", s),
		variable: (s: string) => t.fg("syntaxVariable", s),
		params: (s: string) => t.fg("syntaxVariable", s),
		operator: (s: string) => t.fg("syntaxOperator", s),
		punctuation: (s: string) => t.fg("syntaxPunctuation", s),
		emphasis: (s: string) => t.italic(s),
		strong: (s: string) => t.bold(s),
		link: (s: string) => t.underline(s),
		addition: (s: string) => t.fg("toolDiffAdded", s),
		deletion: (s: string) => t.fg("toolDiffRemoved", s),
	};
}

function getCliHighlightTheme(t: Theme): CliHighlightTheme {
	if (cachedHighlightThemeFor !== t || !cachedCliHighlightTheme) {
		cachedHighlightThemeFor = t;
		cachedCliHighlightTheme = buildCliHighlightTheme(t);
	}
	return cachedCliHighlightTheme;
}

/**
 * Highlight code with syntax coloring based on file extension or language.
 * Returns array of highlighted lines.
 */
export function highlightCode(code: string, lang?: string): string[] {
	// Validate language before highlighting to avoid stderr spam from cli-highlight
	const validLang = lang && supportsLanguage(lang) ? lang : undefined;
	// Skip highlighting when no valid language is specified. cli-highlight's
	// auto-detection is unreliable and can misidentify prose as AppleScript,
	// LiveCodeServer, etc., coloring random English words as keywords.
	if (!validLang) {
		return code.split("\n").map((line) => theme.fg("mdCodeBlock", line));
	}
	const opts = {
		language: validLang,
		ignoreIllegals: true,
		theme: getCliHighlightTheme(theme),
	};
	try {
		return highlight(code, opts).split("\n");
	} catch {
		return code.split("\n");
	}
}

/**
 * Get language identifier from file path extension.
 */
export function getLanguageFromPath(filePath: string): string | undefined {
	const ext = filePath.split(".").pop()?.toLowerCase();
	if (!ext) return undefined;

	const extToLang: Record<string, string> = {
		ts: "typescript",
		tsx: "typescript",
		js: "javascript",
		jsx: "javascript",
		mjs: "javascript",
		cjs: "javascript",
		py: "python",
		rb: "ruby",
		rs: "rust",
		go: "go",
		java: "java",
		kt: "kotlin",
		swift: "swift",
		c: "c",
		h: "c",
		cpp: "cpp",
		cc: "cpp",
		cxx: "cpp",
		hpp: "cpp",
		cs: "csharp",
		php: "php",
		sh: "bash",
		bash: "bash",
		zsh: "bash",
		fish: "fish",
		ps1: "powershell",
		sql: "sql",
		html: "html",
		htm: "html",
		css: "css",
		scss: "scss",
		sass: "sass",
		less: "less",
		json: "json",
		yaml: "yaml",
		yml: "yaml",
		toml: "toml",
		xml: "xml",
		md: "markdown",
		markdown: "markdown",
		dockerfile: "dockerfile",
		makefile: "makefile",
		cmake: "cmake",
		lua: "lua",
		perl: "perl",
		r: "r",
		scala: "scala",
		clj: "clojure",
		ex: "elixir",
		exs: "elixir",
		erl: "erlang",
		hs: "haskell",
		ml: "ocaml",
		vim: "vim",
		graphql: "graphql",
		proto: "protobuf",
		tf: "hcl",
		hcl: "hcl",
	};

	return extToLang[ext];
}

export function getMarkdownTheme(): MarkdownTheme {
	return {
		heading: (text: string) => theme.fg("mdHeading", text),
		link: (text: string) => theme.fg("mdLink", text),
		linkUrl: (text: string) => theme.fg("mdLinkUrl", text),
		code: (text: string) => theme.fg("mdCode", text),
		codeBlock: (text: string) => theme.fg("mdCodeBlock", text),
		codeBlockBorder: (text: string) => theme.fg("mdCodeBlockBorder", text),
		quote: (text: string) => theme.fg("mdQuote", text),
		quoteBorder: (text: string) => theme.fg("mdQuoteBorder", text),
		hr: (text: string) => theme.fg("mdHr", text),
		listBullet: (text: string) => theme.fg("mdListBullet", text),
		bold: (text: string) => theme.bold(text),
		italic: (text: string) => theme.italic(text),
		underline: (text: string) => theme.underline(text),
		strikethrough: (text: string) => chalk.strikethrough(text),
		highlightCode: (code: string, lang?: string): string[] => {
			// Validate language before highlighting to avoid stderr spam from cli-highlight
			const validLang = lang && supportsLanguage(lang) ? lang : undefined;
			// Skip highlighting when no valid language is specified. cli-highlight's
			// auto-detection is unreliable and can misidentify prose as AppleScript,
			// LiveCodeServer, etc., coloring random English words as keywords.
			if (!validLang) {
				return code.split("\n").map((line) => theme.fg("mdCodeBlock", line));
			}
			const opts = {
				language: validLang,
				ignoreIllegals: true,
				theme: getCliHighlightTheme(theme),
			};
			try {
				return highlight(code, opts).split("\n");
			} catch {
				return code.split("\n").map((line) => theme.fg("mdCodeBlock", line));
			}
		},
	};
}

export function getSelectListTheme(): SelectListTheme {
	return {
		selectedPrefix: (text: string) => theme.fg("accent", text),
		selectedText: (text: string) => theme.fg("accent", text),
		description: (text: string) => theme.fg("muted", text),
		scrollInfo: (text: string) => theme.fg("muted", text),
		noMatch: (text: string) => theme.fg("muted", text),
	};
}

export function getEditorTheme(): EditorTheme {
	return {
		borderColor: (text: string) => theme.fg("borderMuted", text),
		selectList: getSelectListTheme(),
	};
}

export function getSettingsListTheme(): SettingsListTheme {
	return {
		label: (text: string, selected: boolean) => (selected ? theme.fg("accent", text) : text),
		value: (text: string, selected: boolean) => (selected ? theme.fg("accent", text) : theme.fg("muted", text)),
		description: (text: string) => theme.fg("dim", text),
		cursor: theme.fg("accent", "→ "),
		hint: (text: string) => theme.fg("dim", text),
	};
}
