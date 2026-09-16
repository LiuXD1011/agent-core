/**
 * UI-neutral theme data: theme JSON loading, color resolution, and theme name state.
 *
 * Presentation concerns (the ANSI `Theme` class, terminal background queries, file
 * watching) live in `modes/interactive/theme/theme.ts`. HTML export and other non-TUI
 * consumers read theme data from here without importing the interactive mode.
 */

import * as fs from "node:fs";
import * as path from "node:path";
import { getCustomThemesDir, getThemesDir } from "../config.ts";
import { stripBom } from "../utils/text.ts";
import type { SourceInfo } from "./source-info.ts";
import type { ThemeColorValue as ColorValue, ValidatedThemeJson as ThemeJson } from "./theme-json.ts";

export type { ValidatedThemeJson as ThemeJson } from "./theme-json.ts";

export type ThemeJsonValidator = (label: string, json: unknown) => ThemeJson;

let themeJsonValidator: ThemeJsonValidator | undefined;

/**
 * Install full theme validation. Without it, documents are accepted as-is, which is what built-in
 * themes already do: validating user-authored JSON needs typebox, and a presentation that only uses
 * built-in themes should not pay ~17 MB of module graph for it.
 */
export function setThemeJsonValidator(validator: ThemeJsonValidator): void {
	themeJsonValidator = validator;
}

export type ThemeColor =
	| "accent"
	| "border"
	| "borderAccent"
	| "borderMuted"
	| "success"
	| "error"
	| "warning"
	| "muted"
	| "dim"
	| "text"
	| "thinkingText"
	| "scrollbarTrack"
	| "scrollbarThumb"
	| "searchMatchText"
	| "userMessageText"
	| "customMessageText"
	| "customMessageLabel"
	| "toolTitle"
	| "toolOutput"
	| "mdHeading"
	| "mdLink"
	| "mdLinkUrl"
	| "mdCode"
	| "mdCodeBlock"
	| "mdCodeBlockBorder"
	| "mdQuote"
	| "mdQuoteBorder"
	| "mdHr"
	| "mdListBullet"
	| "toolDiffAdded"
	| "toolDiffRemoved"
	| "toolDiffContext"
	| "syntaxComment"
	| "syntaxKeyword"
	| "syntaxFunction"
	| "syntaxVariable"
	| "syntaxString"
	| "syntaxNumber"
	| "syntaxType"
	| "syntaxOperator"
	| "syntaxPunctuation"
	| "thinkingOff"
	| "thinkingMinimal"
	| "thinkingLow"
	| "thinkingMedium"
	| "thinkingHigh"
	| "thinkingXhigh"
	| "thinkingMax"
	| "bashMode";

export type ThemeBg =
	| "selectedBg"
	| "searchMatchBg"
	| "userMessageBg"
	| "customMessageBg"
	| "toolPendingBg"
	| "toolSuccessBg"
	| "toolErrorBg";

export type TerminalTheme = "dark" | "light";

export interface RgbColor {
	r: number;
	g: number;
	b: number;
}

// ============================================================================
// Color Utilities
// ============================================================================

function hexToRgb(hex: string): { r: number; g: number; b: number } {
	const cleaned = hex.replace("#", "");
	if (cleaned.length !== 6) {
		throw new Error(`Invalid hex color: ${hex}`);
	}
	const r = Number.parseInt(cleaned.substring(0, 2), 16);
	const g = Number.parseInt(cleaned.substring(2, 4), 16);
	const b = Number.parseInt(cleaned.substring(4, 6), 16);
	if (Number.isNaN(r) || Number.isNaN(g) || Number.isNaN(b)) {
		throw new Error(`Invalid hex color: ${hex}`);
	}
	return { r, g, b };
}

function resolveVarRefs(
	value: ColorValue,
	vars: Record<string, ColorValue>,
	visited = new Set<string>(),
): string | number {
	if (typeof value === "number" || value === "" || value.startsWith("#")) {
		return value;
	}
	if (visited.has(value)) {
		throw new Error(`Circular variable reference detected: ${value}`);
	}
	if (!(value in vars)) {
		throw new Error(`Variable reference not found: ${value}`);
	}
	visited.add(value);
	return resolveVarRefs(vars[value], vars, visited);
}

export function resolveThemeColors<T extends Record<string, ColorValue>>(
	colors: T,
	vars: Record<string, ColorValue> = {},
): Record<keyof T, string | number> {
	const resolved: Record<string, string | number> = {};
	for (const [key, value] of Object.entries(colors)) {
		resolved[key] = resolveVarRefs(value, vars);
	}
	return resolved as Record<keyof T, string | number>;
}

export function withThemeColorFallbacks(colors: ThemeJson["colors"]): ThemeJson["colors"] & {
	scrollbarTrack: ColorValue;
	scrollbarThumb: ColorValue;
	thinkingMax: ColorValue;
	searchMatchBg: ColorValue;
	searchMatchText: ColorValue;
} {
	return {
		...colors,
		scrollbarTrack: colors.scrollbarTrack ?? colors.muted,
		scrollbarThumb: colors.scrollbarThumb ?? colors.text,
		thinkingMax: colors.thinkingMax ?? colors.thinkingXhigh,
		searchMatchBg: colors.searchMatchBg ?? colors.selectedBg,
		searchMatchText: colors.searchMatchText ?? colors.text,
	};
}

// ============================================================================
// Theme JSON Loading
// ============================================================================

let BUILTIN_THEMES: Record<string, ThemeJson> | undefined;

function getBuiltinThemes(): Record<string, ThemeJson> {
	if (!BUILTIN_THEMES) {
		const themesDir = getThemesDir();
		const darkPath = path.join(themesDir, "dark.json");
		const lightPath = path.join(themesDir, "light.json");
		BUILTIN_THEMES = {
			dark: JSON.parse(stripBom(fs.readFileSync(darkPath, "utf-8"))) as ThemeJson,
			light: JSON.parse(stripBom(fs.readFileSync(lightPath, "utf-8"))) as ThemeJson,
		};
	}
	return BUILTIN_THEMES;
}

export interface ThemeInfo {
	name: string;
	path: string | undefined;
}

export function getAvailableThemes(): string[] {
	return getAvailableThemesWithPaths().map(({ name }) => name);
}

export function getAvailableThemesWithPaths(): ThemeInfo[] {
	const themesDir = getThemesDir();
	const result: ThemeInfo[] = [];
	const seen = new Set<string>();
	const addTheme = (themeInfo: ThemeInfo) => {
		if (seen.has(themeInfo.name)) {
			return;
		}
		seen.add(themeInfo.name);
		result.push(themeInfo);
	};

	// Built-in themes
	for (const name of Object.keys(getBuiltinThemes())) {
		addTheme({ name, path: path.join(themesDir, `${name}.json`) });
	}

	// Custom themes
	for (const themeInfo of getCustomThemeInfos()) {
		addTheme(themeInfo);
	}

	for (const [name, sourcePath] of registeredThemeSources.entries()) {
		addTheme({ name, path: sourcePath });
	}

	return result.sort((a, b) => a.name.localeCompare(b.name));
}

function getCustomThemeInfos(): ThemeInfo[] {
	const customThemesDir = getCustomThemesDir();
	const result: ThemeInfo[] = [];
	if (!fs.existsSync(customThemesDir)) {
		return result;
	}

	for (const file of fs.readdirSync(customThemesDir)) {
		if (!file.endsWith(".json")) {
			continue;
		}
		const themePath = path.join(customThemesDir, file);
		try {
			const customTheme = parseThemeJsonContent(themePath, fs.readFileSync(themePath, "utf-8"));
			if (customTheme.name) {
				result.push({ name: customTheme.name, path: themePath });
			}
		} catch {
			// Invalid themes are ignored here; the resource loader reports them
			// during normal startup/reload.
		}
	}
	return result;
}

export function assertThemeNameIsValid(name: string): void {
	if (name.includes("/")) {
		throw new Error(
			`Invalid theme name "${name}": theme names cannot contain "/" because it is reserved for automatic light/dark theme settings.`,
		);
	}
}

function parseThemeJson(label: string, json: unknown): ThemeJson {
	if (themeJsonValidator) return themeJsonValidator(label, json);
	if (typeof json !== "object" || json === null || !("colors" in json)) {
		throw new Error(`Invalid theme "${label}": expected an object with a "colors" map.`);
	}
	return json as ThemeJson;
}

export function parseThemeJsonContent(label: string, content: string): ThemeJson {
	let json: unknown;
	try {
		json = JSON.parse(stripBom(content));
	} catch (error) {
		throw new Error(`Failed to parse theme ${label}: ${error}`);
	}
	return parseThemeJson(label, json);
}

export function loadThemeJson(name: string): ThemeJson {
	const builtinThemes = getBuiltinThemes();
	if (name in builtinThemes) {
		return builtinThemes[name];
	}
	const registeredSource = registeredThemeSources.get(name);
	if (registeredSource) {
		const content = fs.readFileSync(registeredSource, "utf-8");
		return parseThemeJsonContent(registeredSource, content);
	}
	if (registeredThemeSources.has(name)) {
		throw new Error(`Theme "${name}" does not have a source path for export`);
	}
	const customThemesDir = getCustomThemesDir();
	const themePath = path.join(customThemesDir, `${name}.json`);
	if (!fs.existsSync(themePath)) {
		throw new Error(`Theme not found: ${name}`);
	}
	const content = fs.readFileSync(themePath, "utf-8");
	return parseThemeJsonContent(name, content);
}

// ============================================================================
// Theme Name Registry
// ============================================================================

/** Source paths of resource-loaded themes, shared with the TUI theme registry. */
const registeredThemeSources = new Map<string, string | undefined>();

export function setRegisteredThemeSources(themes: ReadonlyArray<{ name?: string; sourcePath?: string }>): void {
	registeredThemeSources.clear();
	for (const theme of themes) {
		if (theme.name) {
			assertThemeNameIsValid(theme.name);
			registeredThemeSources.set(theme.name, theme.sourcePath);
		}
	}
}

let currentThemeName: string | undefined;

export function setCurrentThemeName(name: string | undefined): void {
	currentThemeName = name;
}

export function getCurrentThemeName(): string | undefined {
	return currentThemeName;
}

// ============================================================================
// Theme Settings Parsing
// ============================================================================

export function parseAutoThemeSetting(
	themeSetting: string | undefined,
): { lightTheme: string; darkTheme: string } | undefined {
	if (!themeSetting) return undefined;
	const slashIndex = themeSetting.indexOf("/");
	if (slashIndex === -1 || themeSetting.indexOf("/", slashIndex + 1) !== -1) {
		return undefined;
	}

	const lightTheme = themeSetting.slice(0, slashIndex).trim();
	const darkTheme = themeSetting.slice(slashIndex + 1).trim();
	if (!lightTheme || !darkTheme) {
		return undefined;
	}
	return { lightTheme, darkTheme };
}

export function resolveThemeSetting(
	themeSetting: string | undefined,
	terminalTheme: TerminalTheme,
): string | undefined {
	const autoTheme = parseAutoThemeSetting(themeSetting);
	if (autoTheme) {
		return terminalTheme === "light" ? autoTheme.lightTheme : autoTheme.darkTheme;
	}
	if (themeSetting?.includes("/")) return undefined;
	if (typeof themeSetting === "string") return themeSetting;
	return undefined;
}

// ============================================================================
// Environment-Based Theme Detection
// ============================================================================

function getColorFgBgBackgroundIndex(colorfgbg: string): number | undefined {
	const parts = colorfgbg.split(";");
	const bg = Number.parseInt(parts[1]?.trim() ?? "", 10);
	if (Number.isInteger(bg) && bg >= 0 && bg <= 255) {
		return bg;
	}
	return undefined;
}

function getRgbColorLuminance({ r, g, b }: RgbColor): number {
	const toLinear = (channel: number) => {
		const value = channel / 255;
		return value <= 0.03928 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4;
	};
	return 0.2126 * toLinear(r) + 0.7152 * toLinear(g) + 0.0722 * toLinear(b);
}

/**
 * Convert a 256-color index to hex string.
 * Indices 0-15: basic colors (approximate)
 * Indices 16-231: 6x6x6 color cube
 * Indices 232-255: grayscale ramp
 */
function ansi256ToHex(index: number): string {
	// Basic colors (0-15) - approximate common terminal values
	const basicColors = [
		"#000000",
		"#800000",
		"#008000",
		"#808000",
		"#000080",
		"#800080",
		"#008080",
		"#c0c0c0",
		"#808080",
		"#ff0000",
		"#00ff00",
		"#ffff00",
		"#0000ff",
		"#ff00ff",
		"#00ffff",
		"#ffffff",
	];
	if (index < 16) {
		return basicColors[index];
	}

	// Color cube (16-231): 6x6x6 = 216 colors
	if (index < 232) {
		const cubeIndex = index - 16;
		const r = Math.floor(cubeIndex / 36);
		const g = Math.floor((cubeIndex % 36) / 6);
		const b = cubeIndex % 6;
		const toHex = (n: number) => (n === 0 ? 0 : 55 + n * 40).toString(16).padStart(2, "0");
		return `#${toHex(r)}${toHex(g)}${toHex(b)}`;
	}

	// Grayscale (232-255): 24 shades
	const gray = 8 + (index - 232) * 10;
	const grayHex = gray.toString(16).padStart(2, "0");
	return `#${grayHex}${grayHex}${grayHex}`;
}

function getAnsiColorLuminance(index: number): number {
	return getRgbColorLuminance(hexToRgb(ansi256ToHex(index)));
}

export function getThemeForRgbColor(rgb: RgbColor): TerminalTheme {
	return getRgbColorLuminance(rgb) >= 0.5 ? "light" : "dark";
}

export interface TerminalThemeDetection {
	theme: TerminalTheme;
	source: "terminal background" | "COLORFGBG" | "fallback";
	detail: string;
	confidence: "high" | "low";
}

export interface TerminalThemeDetectionOptions {
	env?: NodeJS.ProcessEnv;
}

export function detectTerminalBackgroundFromEnv(options: TerminalThemeDetectionOptions = {}): TerminalThemeDetection {
	const env = options.env ?? process.env;
	const colorfgbg = env.COLORFGBG || "";
	const bg = getColorFgBgBackgroundIndex(colorfgbg);
	if (bg !== undefined) {
		return {
			theme: getAnsiColorLuminance(bg) >= 0.5 ? "light" : "dark",
			source: "COLORFGBG",
			detail: `background color index ${bg}`,
			confidence: "high",
		};
	}

	return {
		theme: "dark",
		source: "fallback",
		detail: "no terminal background hint found",
		confidence: "low",
	};
}

export function getDefaultTheme(): string {
	return detectTerminalBackgroundFromEnv().theme;
}

// ============================================================================
// HTML Export Helpers
// ============================================================================

/**
 * Get resolved theme colors as CSS-compatible hex strings.
 * Used by HTML export to generate CSS custom properties.
 */
export function getResolvedThemeColors(themeName?: string): Record<string, string> {
	const name = themeName ?? currentThemeName ?? getDefaultTheme();
	const isLight = name === "light";
	const themeJson = loadThemeJson(name);
	const resolved = resolveThemeColors(withThemeColorFallbacks(themeJson.colors), themeJson.vars);

	// Default text color for empty values (terminal uses default fg color)
	const defaultText = isLight ? "#000000" : "#e5e5e7";

	const cssColors: Record<string, string> = {};
	for (const [key, value] of Object.entries(resolved)) {
		if (typeof value === "number") {
			cssColors[key] = ansi256ToHex(value);
		} else if (value === "") {
			// Empty means default terminal color - use sensible fallback for HTML
			cssColors[key] = defaultText;
		} else {
			cssColors[key] = value;
		}
	}
	return cssColors;
}

/**
 * Check if a theme is a "light" theme (for CSS that needs light/dark variants).
 */
export function isLightTheme(themeName?: string): boolean {
	// Currently just check the name - could be extended to analyze colors
	return themeName === "light";
}

/**
 * Get explicit export colors from theme JSON, if specified.
 * Returns undefined for each color that isn't explicitly set.
 */
export function getThemeExportColors(themeName?: string): {
	pageBg?: string;
	cardBg?: string;
	infoBg?: string;
} {
	const name = themeName ?? currentThemeName ?? getDefaultTheme();
	try {
		const themeJson = loadThemeJson(name);
		const exportSection = themeJson.export;
		if (!exportSection) return {};

		const vars = themeJson.vars ?? {};
		const resolve = (value: ColorValue | undefined): string | undefined => {
			if (value === undefined) return undefined;
			const resolved = resolveVarRefs(value, vars);
			if (typeof resolved === "number") return ansi256ToHex(resolved);
			if (resolved === "") return undefined;
			return resolved;
		};

		return {
			pageBg: resolve(exportSection.pageBg),
			cardBg: resolve(exportSection.cardBg),
			infoBg: resolve(exportSection.infoBg),
		};
	} catch {
		return {};
	}
}

/** A loadable theme resource; terminal instances are constructed only by the UI. */
export interface ThemeResource {
	readonly name?: string;
	readonly sourcePath?: string;
	sourceInfo?: SourceInfo;
	readonly json?: ThemeJson;
}
