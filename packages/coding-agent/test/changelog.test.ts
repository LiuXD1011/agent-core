import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, test } from "vitest";
import {
	type ChangelogEntry,
	compareVersions,
	getNewEntries,
	normalizeChangelogLinks,
	parseChangelog,
} from "../src/utils/changelog.ts";

const entry: ChangelogEntry = {
	major: 0,
	minor: 79,
	patch: 0,
	content: "",
};

describe("normalizeChangelogLinks", () => {
	test("rewrites package-relative changelog links to tag-pinned Agent Core source links", () => {
		const markdown = [
			"[Project Trust](README.md#project-trust)",
			"[Extensions](docs/extensions.md#project_trust)",
			"[Examples](examples/extensions/)",
			"[Root README](../../README.md#supply-chain-hardening)",
		].join("\n");

		expect(normalizeChangelogLinks(markdown, entry)).toBe(
			[
				"[Project Trust](https://github.com/LiuXD1011/agent-core/blob/v0.79.0/packages/coding-agent/README.md#project-trust)",
				"[Extensions](https://github.com/LiuXD1011/agent-core/blob/v0.79.0/packages/coding-agent/docs/extensions.md#project_trust)",
				"[Examples](https://github.com/LiuXD1011/agent-core/tree/v0.79.0/packages/coding-agent/examples/extensions/)",
				"[Root README](https://github.com/LiuXD1011/agent-core/blob/v0.79.0/README.md#supply-chain-hardening)",
			].join("\n"),
		);
	});

	test("preserves upstream repository history links and external links verbatim", () => {
		const markdown = [
			"[#5167](https://github.com/earendil-works/pi-mono/pull/5167)",
			"[#4163](https://github.com/badlogic/pi-mono/issues/4163)",
			"[Upstream fix](https://github.com/earendil-works/pi/issues/9132)",
			"[Agent README](https://github.com/badlogic/pi-mono/blob/main/packages/agent/README.md)",
			"[External](https://example.com/docs)",
			"[Local anchor](#settings)",
		].join("\n");

		// Upstream history links are provenance: they must keep their original URLs.
		expect(normalizeChangelogLinks(markdown, "0.79.0")).toBe(markdown);
	});
});

describe("parseChangelog", () => {
	test("only returns Agent Core entries above the upstream history boundary", () => {
		const dir = mkdtempSync(join(tmpdir(), "pi-changelog-test-"));
		const changelogPath = join(dir, "CHANGELOG.md");
		writeFileSync(
			changelogPath,
			[
				"# Changelog",
				"",
				"## [Unreleased]",
				"",
				"### Added",
				"",
				"- Initial Agent Core release, based on Pi v0.85.1.",
				"",
				"<!-- agent-core:upstream-boundary -->",
				"<!-- Entries below are inherited upstream Pi history. -->",
				"",
				"## [0.85.1] - 2026-09-05",
				"",
				"### Fixed",
				"",
				"- Upstream fix that must never display as a Agent Core release.",
				"",
				"## [0.85.0] - 2026-09-04",
				"",
				"- Older upstream entry.",
				"",
			].join("\n"),
		);

		const entries = parseChangelog(changelogPath);
		expect(entries).toEqual([]);
	});

	test("parses Agent Core prerelease and release entries above the boundary", () => {
		const dir = mkdtempSync(join(tmpdir(), "pi-changelog-test-"));
		const changelogPath = join(dir, "CHANGELOG.md");
		writeFileSync(
			changelogPath,
			[
				"# Changelog",
				"",
				"## [Unreleased]",
				"",
				"### Fixed",
				"",
				"- Unreleased fix.",
				"",
				"## [0.1.0] - 2026-10-01",
				"",
				"- Stable release.",
				"",
				"## [0.1.0-alpha.2] - 2026-09-20",
				"",
				"- Second alpha.",
				"",
				"## [0.1.0-alpha.1] - 2026-09-12",
				"",
				"- Initial Agent Core release, based on Pi v0.85.1.",
				"",
				"<!-- agent-core:upstream-boundary -->",
				"<!-- Entries below are inherited upstream Pi history. -->",
				"",
				"## [0.85.1] - 2026-09-05",
				"",
				"- Upstream fix.",
				"",
			].join("\n"),
		);

		const entries = parseChangelog(changelogPath);
		expect(entries.map((e) => `${e.major}.${e.minor}.${e.patch}${e.prerelease ? `-${e.prerelease}` : ""}`)).toEqual([
			"0.1.0",
			"0.1.0-alpha.2",
			"0.1.0-alpha.1",
		]);
	});
});

describe("compareVersions and getNewEntries", () => {
	const released: ChangelogEntry = { major: 0, minor: 1, patch: 0, content: "" };
	const alpha2: ChangelogEntry = { major: 0, minor: 1, patch: 0, prerelease: "alpha.2", content: "" };
	const alpha1: ChangelogEntry = { major: 0, minor: 1, patch: 0, prerelease: "alpha.1", content: "" };
	const upstream: ChangelogEntry = { major: 0, minor: 85, patch: 1, content: "" };

	test("orders prereleases before their release", () => {
		expect(compareVersions(alpha1, alpha2)).toBeLessThan(0);
		expect(compareVersions(alpha2, released)).toBeLessThan(0);
		expect(compareVersions(alpha2, alpha2)).toBe(0);
		expect(compareVersions(released, alpha1)).toBeGreaterThan(0);
		expect(comparePrereleaseOrder("0.1.0-alpha.10", "0.1.0-alpha.9")).toBeGreaterThan(0);
	});

	test("getNewEntries handles prerelease upgrades, promotion to stable, no-op, and repeated reads", () => {
		const entries = [released, alpha2, alpha1];

		// Prerelease upgrade: 0.1.0-alpha.1 -> 0.1.0-alpha.2 (a released 0.1.0 also counts as new)
		expect(getNewEntries(entries, "0.1.0-alpha.1")).toEqual([released, alpha2]);
		// Promotion to stable: 0.1.0-alpha.2 -> 0.1.0
		expect(getNewEntries(entries, "0.1.0-alpha.2")).toEqual([released]);
		// No new entries: same version again (repeated reads must not re-display)
		expect(getNewEntries(entries, "0.1.0")).toEqual([]);
		expect(getNewEntries(entries, "0.1.0-alpha.2")).toEqual([released]);
		// An upstream-style version string (0.85.1) is only newer if such entries exist
		expect(getNewEntries([...entries, upstream], "0.85.1")).toEqual([]);
	});
});

function comparePrereleaseOrder(a: string, b: string): number {
	return compareVersions({ ...parseTestVersion(a), content: "" }, { ...parseTestVersion(b), content: "" });
}

function parseTestVersion(version: string): Pick<ChangelogEntry, "major" | "minor" | "patch" | "prerelease"> {
	const hyphenIndex = version.indexOf("-");
	const core = hyphenIndex === -1 ? version : version.slice(0, hyphenIndex);
	const parts = core.split(".").map(Number);
	return {
		major: parts[0],
		minor: parts[1],
		patch: parts[2],
		prerelease: hyphenIndex === -1 ? undefined : version.slice(hyphenIndex + 1),
	};
}
