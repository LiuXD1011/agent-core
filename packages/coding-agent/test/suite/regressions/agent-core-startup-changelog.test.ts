import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { SettingsManager } from "../../../src/core/settings-manager.ts";
import { computeStartupChangelog, getChangelogPath, parseChangelog } from "../../../src/utils/changelog.ts";

const BOUNDARY = "<!-- agent-core:upstream-boundary -->";

function writeChangelog(path: string, releases: string[]): void {
	writeFileSync(
		path,
		[
			"# Changelog",
			"",
			"## [Unreleased]",
			"",
			...releases,
			BOUNDARY,
			"<!-- Entries below are inherited upstream Pi history (github.com/earendil-works/pi), kept verbatim for provenance. They are not Agent Core releases. -->",
			"",
			"## [0.85.1] - 2026-09-05",
			"",
			"### Fixed",
			"",
			"- Upstream fix that must never display as Agent Core news ([#9166](https://github.com/earendil-works/pi/pull/9166)).",
			"",
		].join("\n"),
	);
}

describe("Agent Core startup changelog across consecutive fresh sessions", () => {
	const dirs: string[] = [];

	afterEach(() => {
		while (dirs.length > 0) {
			rmSync(dirs.pop() as string, { recursive: true, force: true });
		}
	});

	function makeWorkspace(): { cwd: string; agentDir: string; changelogPath: string } {
		const root = mkdtempSync(join(tmpdir(), "agent-core-startup-changelog-"));
		dirs.push(root);
		const cwd = join(root, "project");
		const agentDir = join(root, "agent");
		mkdirSync(cwd);
		mkdirSync(agentDir);
		const changelogPath = join(root, "CHANGELOG.md");
		return { cwd, agentDir, changelogPath };
	}

	it("records the version on first launch and never re-displays inherited upstream history", async () => {
		const { cwd, agentDir, changelogPath } = makeWorkspace();
		writeChangelog(changelogPath, [
			"## [0.1.0-alpha.1] - 2026-09-12",
			"",
			"- Initial Agent Core release, based on Pi v0.85.1.",
			"",
		]);

		// First fresh session: nothing to show, but the current version is recorded.
		const first = SettingsManager.create(cwd, agentDir);
		expect(first.getLastChangelogVersion()).toBeUndefined();
		const firstResult = computeStartupChangelog(changelogPath, first.getLastChangelogVersion(), "0.1.0-alpha.1");
		expect(firstResult.markdown).toBeUndefined();
		expect(firstResult.seenVersion).toBe("0.1.0-alpha.1");
		first.setLastChangelogVersion("0.1.0-alpha.1");
		await first.flush();

		// Second and third fresh sessions must not display anything.
		for (let launch = 2; launch <= 3; launch++) {
			const settings = SettingsManager.create(cwd, agentDir);
			expect(settings.getLastChangelogVersion()).toBe("0.1.0-alpha.1");
			const result = computeStartupChangelog(changelogPath, settings.getLastChangelogVersion(), "0.1.0-alpha.1");
			expect(result.markdown).toBeUndefined();
			expect(result.seenVersion).toBeUndefined();
		}
	});

	it("shows a new Agent Core release exactly once after an upgrade", async () => {
		const { cwd, agentDir, changelogPath } = makeWorkspace();
		writeChangelog(changelogPath, [
			"## [0.1.0-alpha.1] - 2026-09-12",
			"",
			"- Initial Agent Core release, based on Pi v0.85.1.",
			"",
		]);

		const before = SettingsManager.create(cwd, agentDir);
		before.setLastChangelogVersion("0.1.0-alpha.1");
		await before.flush();

		// Upgrade: a new Agent Core prerelease appears in the changelog.
		writeChangelog(changelogPath, [
			"## [0.1.0-alpha.2] - 2026-09-20",
			"",
			"### Fixed",
			"",
			"- Fixed duplicate startup changelog.",
			"",
			"## [0.1.0-alpha.1] - 2026-09-12",
			"",
			"- Initial Agent Core release, based on Pi v0.85.1.",
			"",
		]);

		const upgraded = SettingsManager.create(cwd, agentDir);
		const result = computeStartupChangelog(changelogPath, upgraded.getLastChangelogVersion(), "0.1.0-alpha.2");
		expect(result.seenVersion).toBe("0.1.0-alpha.2");
		expect(result.markdown).toContain("Fixed duplicate startup changelog.");
		expect(result.markdown).not.toContain("inherited upstream Pi history");
		expect(result.markdown).not.toContain("0.85.1");
		upgraded.setLastChangelogVersion("0.1.0-alpha.2");
		await upgraded.flush();

		// The next launch shows nothing.
		const after = SettingsManager.create(cwd, agentDir);
		const afterResult = computeStartupChangelog(changelogPath, after.getLastChangelogVersion(), "0.1.0-alpha.2");
		expect(afterResult.markdown).toBeUndefined();
		expect(afterResult.seenVersion).toBeUndefined();
	});

	it("keeps the shipped changelog free of upstream entries", () => {
		const entries = parseChangelog(getChangelogPath());
		const versioned = entries.filter((entry) => entry.major !== 0 || entry.minor !== 1);
		expect(versioned).toEqual([]);
	});
});
