import path from "node:path";
import { existsSync, readFileSync } from "fs";

export interface ChangelogEntry {
	major: number;
	minor: number;
	patch: number;
	prerelease?: string;
	content: string;
}

const GITHUB_REPO = "LiuXD1011/agent-core";
const CHANGELOG_LINK_BASE_PATH = "packages/coding-agent";
// Links into the upstream Pi repositories are historical references; they are
// preserved verbatim instead of being retargeted to the Agent Core repository.
const UPSTREAM_REPO_RE = /^https:\/\/github\.com\/(?:badlogic|earendil-works)\/pi(?:-mono)?(?=\/|$)/;
const URL_SCHEME_RE = /^[a-z][a-z0-9+.-]*:/i;
const INLINE_MARKDOWN_LINK_RE = /(!?\[[^\]\n]+\]\()([^\s)]+)((?:\s+[^)]*)?\))/g;
// Changelogs keep inherited upstream Pi history below this marker for
// provenance. Those entries are not Agent Core releases and must never take part
// in "what's new" comparisons, so parsing stops at the marker.
const UPSTREAM_BOUNDARY_LINE = "<!-- agent-core:upstream-boundary -->";

function entryVersion(entry: ChangelogEntry): string {
	const core = `${entry.major}.${entry.minor}.${entry.patch}`;
	return entry.prerelease ? `${core}-${entry.prerelease}` : core;
}

function parseVersion(version: string): Pick<ChangelogEntry, "major" | "minor" | "patch" | "prerelease"> {
	const hyphenIndex = version.indexOf("-");
	const core = hyphenIndex === -1 ? version : version.slice(0, hyphenIndex);
	const prerelease = hyphenIndex === -1 ? undefined : version.slice(hyphenIndex + 1);
	const parts = core.split(".").map(Number);
	return {
		major: parts[0] || 0,
		minor: parts[1] || 0,
		patch: parts[2] || 0,
		prerelease,
	};
}

function comparePrereleaseIdentifiers(a: string, b: string): number {
	const aParts = a.split(".");
	const bParts = b.split(".");

	for (let i = 0; i < Math.max(aParts.length, bParts.length); i++) {
		const aPart = aParts[i];
		const bPart = bParts[i];
		if (aPart === undefined) return -1;
		if (bPart === undefined) return 1;

		const aNumber = /^\d+$/.test(aPart) ? Number(aPart) : undefined;
		const bNumber = /^\d+$/.test(bPart) ? Number(bPart) : undefined;
		if (aNumber !== undefined && bNumber !== undefined) {
			if (aNumber !== bNumber) return aNumber - bNumber;
		} else if (aPart !== bPart) {
			return aPart < bPart ? -1 : 1;
		}
	}

	return 0;
}

function normalizeTag(version: string | ChangelogEntry): string {
	const versionString = typeof version === "string" ? version : entryVersion(version);
	return versionString.startsWith("v") ? versionString : `v${versionString}`;
}

function splitLocalTarget(target: string): { fragment: string; pathPart: string; query: string } {
	const hashIndex = target.indexOf("#");
	const beforeHash = hashIndex === -1 ? target : target.slice(0, hashIndex);
	const fragment = hashIndex === -1 ? "" : target.slice(hashIndex);
	const queryIndex = beforeHash.indexOf("?");

	if (queryIndex === -1) {
		return { fragment, pathPart: beforeHash, query: "" };
	}

	return {
		fragment,
		pathPart: beforeHash.slice(0, queryIndex),
		query: beforeHash.slice(queryIndex),
	};
}

function normalizePathPart(value: string): string {
	return value.replaceAll("\\", "/");
}

function resolveRepositoryPath(targetPath: string): string | undefined {
	const normalizedTarget = normalizePathPart(targetPath);
	const joined = normalizedTarget.startsWith("/")
		? path.posix.normalize(normalizedTarget.replace(/^\/+/, ""))
		: path.posix.normalize(path.posix.join(CHANGELOG_LINK_BASE_PATH, normalizedTarget));

	if (joined === "." || joined.startsWith("../") || joined === "..") {
		return undefined;
	}

	return joined;
}

function isDirectoryTarget(originalPath: string, repositoryPath: string): boolean {
	if (originalPath.endsWith("/")) {
		return true;
	}

	const basename = path.posix.basename(repositoryPath);
	return !basename.includes(".");
}

function normalizeChangelogLinkTarget(target: string, tag: string): string {
	if (UPSTREAM_REPO_RE.test(target)) {
		return target;
	}

	let canonicalTarget = target;
	const repoUrl = `https://github.com/${GITHUB_REPO}`;

	for (const route of ["blob", "tree"]) {
		for (const branch of ["main", "master"]) {
			const floatingRefPrefix = `${repoUrl}/${route}/${branch}/`;
			if (canonicalTarget.startsWith(floatingRefPrefix)) {
				canonicalTarget = `${repoUrl}/${route}/${tag}/${canonicalTarget.slice(floatingRefPrefix.length)}`;
			}
		}
	}

	if (canonicalTarget.startsWith("#") || canonicalTarget.startsWith("//") || URL_SCHEME_RE.test(canonicalTarget)) {
		return canonicalTarget;
	}

	const { fragment, pathPart, query } = splitLocalTarget(canonicalTarget);
	if (!pathPart) {
		return canonicalTarget;
	}

	const repositoryPath = resolveRepositoryPath(pathPart);
	if (!repositoryPath) {
		return canonicalTarget;
	}

	const route = isDirectoryTarget(pathPart, repositoryPath) ? "tree" : "blob";
	return `https://github.com/${GITHUB_REPO}/${route}/${tag}/${encodeURI(repositoryPath)}${query}${fragment}`;
}

export function normalizeChangelogLinks(markdown: string, version: string | ChangelogEntry): string {
	const tag = normalizeTag(version);
	return markdown.replace(INLINE_MARKDOWN_LINK_RE, (_match, prefix, target, suffix) => {
		return `${prefix}${normalizeChangelogLinkTarget(target, tag)}${suffix}`;
	});
}

/**
 * Parse changelog entries from CHANGELOG.md
 * Scans for ## lines and collects content until next ## or EOF.
 * Stops at the Agent Core/upstream history boundary: inherited upstream Pi
 * entries below the marker are provenance, not Agent Core releases.
 */
export function parseChangelog(changelogPath: string): ChangelogEntry[] {
	if (!existsSync(changelogPath)) {
		return [];
	}

	try {
		const content = readFileSync(changelogPath, "utf-8");
		const lines = content.split("\n");
		const entries: ChangelogEntry[] = [];

		let currentLines: string[] = [];
		let currentVersion: ChangelogEntry | null = null;

		for (const line of lines) {
			if (line.trim() === UPSTREAM_BOUNDARY_LINE) {
				break;
			}

			// Check if this is a version header (## [x.y.z] or ## [x.y.z-prerelease] ...)
			if (line.startsWith("## ")) {
				// Save previous entry if exists
				if (currentVersion && currentLines.length > 0) {
					entries.push({ ...currentVersion, content: currentLines.join("\n").trim() });
				}

				// Try to parse version from this line
				const versionMatch = line.match(/##\s+\[?(\d+)\.(\d+)\.(\d+)(?:-([0-9A-Za-z.-]+))?\]?/);
				if (versionMatch) {
					currentVersion = {
						major: Number.parseInt(versionMatch[1], 10),
						minor: Number.parseInt(versionMatch[2], 10),
						patch: Number.parseInt(versionMatch[3], 10),
						prerelease: versionMatch[4],
						content: "",
					};
					currentLines = [line];
				} else {
					// Reset if we can't parse version
					currentVersion = null;
					currentLines = [];
				}
			} else if (currentVersion) {
				// Collect lines for current version
				currentLines.push(line);
			}
		}

		// Save last entry
		if (currentVersion && currentLines.length > 0) {
			entries.push({ ...currentVersion, content: currentLines.join("\n").trim() });
		}

		return entries;
	} catch (error) {
		console.error(`Warning: Could not parse changelog: ${error}`);
		return [];
	}
}

/**
 * Compare versions. Returns: -1 if v1 < v2, 0 if v1 === v2, 1 if v1 > v2.
 * A prerelease (0.1.0-alpha.1) sorts before its release (0.1.0).
 */
export function compareVersions(v1: ChangelogEntry, v2: ChangelogEntry): number {
	if (v1.major !== v2.major) return v1.major - v2.major;
	if (v1.minor !== v2.minor) return v1.minor - v2.minor;
	if (v1.patch !== v2.patch) return v1.patch - v2.patch;
	if (v1.prerelease === v2.prerelease) return 0;
	if (v1.prerelease === undefined) return 1;
	if (v2.prerelease === undefined) return -1;
	return comparePrereleaseIdentifiers(v1.prerelease, v2.prerelease);
}

/**
 * Get entries newer than lastVersion
 */
export function getNewEntries(entries: ChangelogEntry[], lastVersion: string): ChangelogEntry[] {
	const last: ChangelogEntry = {
		...parseVersion(lastVersion),
		content: "",
	};

	return entries.filter((entry) => compareVersions(entry, last) > 0);
}

export interface StartupChangelogResult {
	/** Markdown to display, or undefined when nothing should be shown. */
	markdown?: string;
	/** Version to persist as last seen, or undefined when nothing changed. */
	seenVersion?: string;
}

/**
 * Compute the startup "what's new" markdown for the current product version.
 * Only Agent Core entries (above the upstream history boundary) participate.
 * A fresh install records the version without displaying anything.
 */
export function computeStartupChangelog(
	changelogPath: string,
	lastVersion: string | undefined,
	currentVersion: string,
): StartupChangelogResult {
	if (!lastVersion) {
		// Fresh install: record the version, don't show inherited history.
		return { seenVersion: currentVersion };
	}

	const entries = parseChangelog(changelogPath);
	const newEntries = getNewEntries(entries, lastVersion);
	if (newEntries.length === 0) {
		return {};
	}

	return {
		markdown: newEntries.map((e) => normalizeChangelogLinks(e.content, e)).join("\n\n"),
		seenVersion: currentVersion,
	};
}

// Re-export getChangelogPath from paths.ts for convenience
export { getChangelogPath } from "../config.ts";
