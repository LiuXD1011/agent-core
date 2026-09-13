#!/usr/bin/env node
/**
 * Static branding regression checks for Agent Core.
 *
 * Verifies that product identity (name, CLI command, npm scope, config
 * directory, environment variable prefix, repository, runtime markers, update
 * defaults, and artifact names) stays on Agent Core, while narrowly allowlisting
 * upstream provenance (copyright, history links, retained external services,
 * and task-planning documents).
 *
 * Usage:
 *   node scripts/check-branding.mjs [root]   (default: process.cwd())
 *
 * Exit code 0 = pass, 1 = violations found. The checker never writes the files
 * it inspects.
 */

import { readFileSync, readdirSync, statSync } from "node:fs";
import { join, relative, sep } from "node:path";

const REPO_URL = "git+https://github.com/LiuXD1011/agent-core.git";
const SKIP_DIRECTORIES = new Set(["node_modules", "dist", ".git", "binaries", ".eval", "graphify-out", "vendor"]);

const violations = [];

function fail(file, line, message) {
	violations.push(`${file}:${line}: ${message}`);
}

// ---------------------------------------------------------------------------
// File inventory
// ---------------------------------------------------------------------------

const CODE_EXTENSIONS = [".mjs", ".ps1", ".bat", ".yml", ".yaml"];

function walk(dir, root, entries = []) {
	for (const entry of readdirSync(dir)) {
		if (SKIP_DIRECTORIES.has(entry)) continue;
		const fullPath = join(dir, entry);
		let stats;
		try {
			stats = statSync(fullPath);
		} catch {
			continue;
		}
		if (stats.isDirectory()) {
			walk(fullPath, root, entries);
		} else if (
			entry.endsWith(".md") ||
			entry.endsWith(".json") ||
			entry.endsWith(".sh") ||
			entry.endsWith(".ts") ||
			CODE_EXTENSIONS.some((extension) => entry.endsWith(extension))
		) {
			entries.push(fullPath);
		}
	}
	return entries;
}

function relPath(fullPath, root) {
	return relative(root, fullPath).split(sep).join("/");
}

// ---------------------------------------------------------------------------
// Allowlists: narrow, per-file, with reasons. Each entry lists the patterns it
// neutralizes for matching lines. Broad directory-level exemptions are not used.
// ---------------------------------------------------------------------------

const upstreamLinkPattern = /(github\.com\/(?:badlogic|earendil-works)|pi\.dev)/;
const oldEnvPattern = /\bPI_[A-Z][A-Z_]*\b/;
const upstreamInstallPattern = /npm install -g(?: --ignore-scripts)? @earendil-works\//;
const installerScriptPattern = /pi\.dev\/install\.sh/;
const piCoreSelfReferencePattern = /\bPi Core\b/;
const oldLaunchCommandPattern = /\bpi (?:-e|--extension|--mode|--preset|-p)\b/;
const upstreamSchemaUrlPattern = /raw\.githubusercontent\.com\/earendil-works\/pi\//;

/** Markdown allowlist: file -> { lineIncludes, pattern, reason }[] */
const markdownAllowlist = {
	"CONTRIBUTING.md": [
		{
			lineIncludes: "v0.85.1",
			pattern: upstreamLinkPattern,
			reason: "provenance statement linking upstream Pi",
		},
	],
	"README.md": [
		{
			lineIncludes: "secondary development",
			pattern: upstreamLinkPattern,
			reason: "attribution link to upstream Pi",
		},
		{
			lineIncludes: "credit for the original design",
			pattern: upstreamLinkPattern,
			reason: "attribution to the Pi authors",
		},
		{
			lineIncludes: "coexist with an existing `pi` installation",
			pattern: upstreamLinkPattern,
			reason: "provenance link to upstream Pi in the package README",
		},
	],
	"SECURITY.md": [
		{
			lineIncludes: "upstream project",
			pattern: upstreamLinkPattern,
			reason: "pointer for reporting issues that affect the upstream project",
		},
	],
	"NOTICE.md": [
		{ lineIncludes: "earendil-works", pattern: upstreamLinkPattern, reason: "provenance notice" },
	],
	"packages/coding-agent/README.md": [
		{
			lineIncludes: "secondary development of",
			pattern: upstreamLinkPattern,
			reason: "attribution link to upstream Pi",
		},
	],
	"packages/coding-agent/docs/index.md": [
		{
			lineIncludes: "secondary development of",
			pattern: upstreamLinkPattern,
			reason: "attribution link to upstream Pi",
		},
	],
	"packages/coding-agent/docs/development.md": [
		{
			lineIncludes: "pi.dev gallery",
			pattern: upstreamLinkPattern,
			reason: "branding-exception documentation naming the retained third-party service",
		},
	],
	"packages/coding-agent/docs/packages.md": [
		{
			lineIncludes: "pi.dev/packages",
			pattern: upstreamLinkPattern,
			reason: "retained upstream gallery, documented as an external service",
		},
	],
	"packages/coding-agent/docs/usage.md": [
		{
			lineIncludes: "pi-share-hf",
			pattern: upstreamLinkPattern,
			reason: "third-party session publishing tool, attributed",
		},
	],
	"packages/coding-agent/docs/skills.md": [
		{
			lineIncludes: "pi-skills",
			pattern: upstreamLinkPattern,
			reason: "third-party skills collection, attributed",
		},
	],
	"packages/coding-agent/docs/containerization.md": [
		{
			lineIncludes: "Gondolin",
			pattern: upstreamLinkPattern,
			reason: "third-party sandbox product (earendil-works/gondolin), a real dependency",
		},
	],
	"packages/coding-agent/examples/extensions/doom-overlay/README.md": [
		{
			lineIncludes: "pi-doom",
			pattern: upstreamLinkPattern,
			reason: "third-party original integration credit in an example",
		},
	],
};

/** Files whose entire markdown content is exempt (history or planning docs). */
const exemptMarkdownFiles = new Set([
	"docs/修改方案.md", // local audit handoff quoting old names as findings, not instructions
]);

function isExemptMarkdown(rel) {
	return exemptMarkdownFiles.has(rel);
}

/**
 * Released changelog sections are immutable history; only the Unreleased
 * section is held to current branding. The protected range starts at the
 * explicit boundary marker or the first released version header.
 */
function unreleasedChangelogLines(lines) {
	const boundary = lines.findIndex(
		(line) => line.includes("agent-core:upstream-boundary") || /^## \[\d+\.\d/.test(line),
	);
	return boundary === -1 ? lines : lines.slice(0, boundary);
}

function allowedBy(line, entries) {
	return entries?.some((entry) => line.includes(entry.lineIncludes) && entry.pattern.test(line));
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/** Unreleased-changelog allowlist: file -> { lineIncludes, pattern, reason }[] */
const changelogUnreleasedAllowlist = {
	"packages/coding-agent/CHANGELOG.md": [
		{
			lineIncludes: "to `AGENT_CORE_*`",
			pattern: oldEnvPattern,
			reason: "breaking-change entry describing the environment-variable rename",
		},
		{
			lineIncludes: "Removed the pi.dev",
			pattern: upstreamLinkPattern,
			reason: "removal note for the disabled upstream catalog contact",
		},
	],
};

function checkMarkdown(files, root) {
	// Schema URLs also live in JSON resource fixtures; scan those narrowly.
	for (const fullPath of files.filter((file) => file.endsWith(".json") && !file.endsWith("package.json") && !file.endsWith("package-lock.json"))) {
		const rel = relPath(fullPath, root);
		if (isExemptMarkdown(rel)) continue;
		const lines = readFileSync(fullPath, "utf8").split("\n");
		lines.forEach((line, index) => {
			if (upstreamSchemaUrlPattern.test(line)) {
				fail(rel, index + 1, "schema URL must reference the LiuXD1011/agent-core repository");
			}
		});
	}

	for (const fullPath of files.filter((file) => file.endsWith(".md"))) {
		const rel = relPath(fullPath, root);
		if (isExemptMarkdown(rel)) continue;

		const isChangelog = rel === "CHANGELOG.md" || rel.endsWith("/CHANGELOG.md");
		let lines = readFileSync(fullPath, "utf8").split("\n");
		if (isChangelog) {
			lines = unreleasedChangelogLines(lines);
		}
		lines.forEach((line, index) => {
			const lineNumber = index + 1;
			const allowlist = isChangelog ? changelogUnreleasedAllowlist[rel] : markdownAllowlist[rel];
			if (upstreamInstallPattern.test(line)) {
				fail(rel, lineNumber, "upstream package used as the Agent Core install command");
			}
			if (installerScriptPattern.test(line)) {
				fail(rel, lineNumber, "upstream installer script presented as a Agent Core install option");
			}
			if (oldEnvPattern.test(line) && !allowedBy(line, allowlist)) {
				fail(rel, lineNumber, `legacy pre-rename environment variable (expected AGENT_CORE_*): ${oldEnvPattern.exec(line)[0]}`);
			}
			if (piCoreSelfReferencePattern.test(line)) {
				fail(rel, lineNumber, "current distribution must not self-identify as Pi Core");
			}
			if (oldLaunchCommandPattern.test(line)) {
				fail(rel, lineNumber, "current documentation must not launch the old pi command");
			}
			if (upstreamSchemaUrlPattern.test(line)) {
				fail(rel, lineNumber, "schema URL must reference the LiuXD1011/agent-core repository");
			}
			if (upstreamLinkPattern.test(line) && !allowedBy(line, allowlist)) {
				fail(rel, lineNumber, "current documentation must link to LiuXD1011/agent-core, not upstream services");
			}
		});
	}
}

function checkWorkspaceMetadata(files, root) {
	for (const fullPath of files.filter((file) => file.endsWith("package.json"))) {
		const rel = relPath(fullPath, root);
		// Example extensions use fictional third-party package names and doc
		// fixtures are not brand-bearing packages.
		if (rel.includes("/examples/") || rel.includes("/docs/")) continue;

		let pkg;
		try {
			pkg = JSON.parse(readFileSync(fullPath, "utf8"));
		} catch {
			fail(rel, 1, "unparseable package.json");
			continue;
		}
		if (pkg.private === true && !pkg.name?.startsWith("@liuxuedeng/")) {
			// Fixtures and generated manifests are out of scope.
			continue;
		}
		if (!/^@liuxuedeng\/agent-core(-|$)/.test(pkg.name ?? "")) {
			fail(rel, 1, `unexpected package name: ${pkg.name}`);
		}
		if (pkg.author !== undefined && pkg.author !== "LiuXD1011") {
			fail(rel, 1, `package author must be LiuXD1011, found: ${pkg.author}`);
		}
		if (pkg.repository !== undefined && pkg.repository.url !== REPO_URL) {
			fail(rel, 1, `repository must point at ${REPO_URL}, found: ${pkg.repository.url}`);
		}
		for (const binName of Object.keys(pkg.bin ?? {})) {
			if (binName === "pi" || binName === "pi-core") {
				fail(rel, 1, `legacy CLI binary name "${binName}" must not be reintroduced`);
			}
		}
		if (/\bPi (?:packages|coding agent|Core)\b/i.test(pkg.description ?? "")) {
			fail(rel, 1, `package description must not self-identify with the old brand: ${pkg.description}`);
		}
	}
}

function checkSource(files, root) {
	const sourceChecks = [
		{
			file: "packages/coding-agent/src/cli/setup.ts",
			pattern: /AI_AGENT = "pi"/,
			message: 'CLI must set AI_AGENT=agent-core (via APP_NAME), not "pi"',
		},
		{
			file: "packages/coding-agent/src/rpc-entry.ts",
			pattern: /AI_AGENT = "pi"/,
			message: 'RPC entry must set AI_AGENT=agent-core (via APP_NAME), not "pi"',
		},
		{
			file: "packages/coding-agent/src/package-manager-cli.ts",
			pattern: upstreamLinkPattern,
			message: "managed installs must not implicitly use an upstream installer endpoint",
		},
		{
			file: "packages/coding-agent/src/modes/interactive/theme/dark.json",
			pattern: upstreamLinkPattern,
			message: "theme schema should reference the Agent Core repository",
		},
		{
			file: "packages/coding-agent/src/modes/interactive/theme/light.json",
			pattern: upstreamLinkPattern,
			message: "theme schema should reference the Agent Core repository",
		},
		{
			file: "scripts/build-binaries.sh",
			pattern: /pi(-\$platform|\$\{platform\})|\/pi(\.exe)?"|\/pi" /,
			message: "binaries and archives must be named agent-core[.exe]",
		},
		{
			file: "packages/ai/src/utils/pi-user-agent.ts",
			pattern: /`pi \(|pi \(browser"/,
			message: "AI package User-Agent must use the agent-core brand on Node and browser paths",
		},
		{
			file: "packages/coding-agent/src/cli/auth-command.ts",
			pattern: /\bpi auth\b/,
			message: "auth command help must use APP_NAME, not a hardcoded pi command",
		},
		{
			file: "packages/coding-agent/package.json",
			pattern: /--outfile dist\/pi\b/,
			message: "build:binary must output dist/agent-core, not dist/pi",
		},
		{
			file: "scripts/create-source-archive.sh",
			pattern: /archive_root="pi-/,
			message: "source archive root must be agent-core-${version}",
		},
	];

	for (const check of sourceChecks) {
		const fullPath = join(root, check.file);
		if (!existsSyncSafe(fullPath)) continue;
		const lines = readFileSync(fullPath, "utf8").split("\n");
		lines.forEach((line, index) => {
			if (check.pattern.test(line)) {
				fail(check.file, index + 1, check.message);
			}
		});
	}

	// Retained upstream services: allowed only on their documented definition lines.
	const retainedServiceLines = [
		{
			file: "packages/coding-agent/src/config.ts",
			lineIncludes: "DEFAULT_SHARE_VIEWER_URL",
			message: "unexpected upstream service reference outside the retained share-viewer default",
		},
		{
			file: "packages/coding-agent/src/utils/version-check.ts",
			lineIncludes: "upstream pi.dev release feed",
			message: "unexpected upstream feed reference outside the decoupling-boundary comment",
		},
		{
			file: "packages/coding-agent/src/modes/interactive/interactive-mode.ts",
			lineIncludes: "upstream pi.dev telemetry endpoint",
			message: "unexpected upstream service reference outside the telemetry-boundary comment",
		},
		{
			file: "packages/coding-agent/src/utils/changelog.ts",
			lineIncludes: "UPSTREAM_REPO_RE",
			message: "unexpected upstream repository reference outside the link-protection regex",
		},
	];
	for (const entry of retainedServiceLines) {
		const fullPath = join(root, entry.file);
		if (!existsSyncSafe(fullPath)) continue;
		const lines = readFileSync(fullPath, "utf8").split("\n");
		lines.forEach((line, index) => {
			if (upstreamLinkPattern.test(line) && !line.includes(entry.lineIncludes)) {
				fail(entry.file, index + 1, entry.message);
			}
		});
	}
}

/** Allowlist for code files (mjs/ps1/bat/yml): file -> { lineIncludes, pattern, reason }[] */
const codeAllowlist = {
	".github/workflows/ci.yml": [
		{
			lineIncludes: "pi-mono",
			pattern: upstreamLinkPattern,
			reason: "CI notification routes to the upstream mirror repository",
		},
	],
};

function checkCodeFiles(files, root) {
	// Negative branding tests quote old names as fixture content, and the
	// checker defines the patterns it enforces; scanning them would only
	// match their own bad samples.
	const codeFiles = files.filter(
		(file) =>
			CODE_EXTENSIONS.some((extension) => file.endsWith(extension)) &&
			!file.endsWith(".test.mjs") &&
			relPath(file, root) !== "scripts/check-branding.mjs",
	);
	for (const fullPath of codeFiles) {
		const rel = relPath(fullPath, root);
		const lines = readFileSync(fullPath, "utf8").split("\n");
		lines.forEach((line, index) => {
			const lineNumber = index + 1;
			if (upstreamInstallPattern.test(line)) {
				fail(rel, lineNumber, "upstream package used as the Agent Core install command");
			}
			if (upstreamLinkPattern.test(line) && !allowedBy(line, codeAllowlist[rel])) {
				fail(rel, lineNumber, "build and workflow scripts must not reference upstream services");
			}
		});
	}
}

function existsSyncSafe(path) {
	try {
		statSync(path);
		return true;
	} catch {
		return false;
	}
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

export function collectViolations(root) {
	const files = walk(root, root);
	checkMarkdown(files, root);
	checkWorkspaceMetadata(files, root);
	checkSource(files, root);
	checkCodeFiles(files, root);
	return violations;
}

function main() {
	const root = process.argv[2] ?? process.cwd();
	collectViolations(root);

	if (violations.length > 0) {
		console.error(`Branding check failed with ${violations.length} violation(s):`);
		for (const violation of violations) {
			console.error(`  ${violation}`);
		}
		process.exit(1);
	}
	console.log("Branding check passed.");
}

if (process.argv[1] && process.argv[1].endsWith("check-branding.mjs")) {
	main();
}
