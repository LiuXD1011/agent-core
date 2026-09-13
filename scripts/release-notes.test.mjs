import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const releaseNotesScript = fileURLToPath(new URL("./release-notes.mjs", import.meta.url));

function makeGhShim(binDirectory) {
	const shim = join(binDirectory, "gh");
	writeFileSync(
		shim,
		`#!/usr/bin/env node
const fs = require("node:fs");
const args = process.argv.slice(2);
fs.appendFileSync(process.env.GH_CALL_LOG, JSON.stringify(args) + "\\n");
if (args[0] === "api" && /\\/releases$/.test(args[1])) {
	process.stdout.write(fs.readFileSync(process.env.GH_RELEASES_FILE, "utf8"));
}
`,
	);
	chmodSync(shim, 0o755);
}

function makeWorkspace() {
	const root = mkdtempSync(join(tmpdir(), "pi-release-notes-test-"));
	const binDirectory = join(root, "bin");
	mkdirSync(binDirectory);
	makeGhShim(binDirectory);
	const callLogPath = join(root, "gh-calls.log");
	const releasesPath = join(root, "releases.jsonl");
	return {
		root,
		binDirectory,
		callLogPath,
		releasesPath,
		cleanup: () => rmSync(root, { recursive: true, force: true }),
	};
}

function runFixReleases({ binDirectory, callLogPath, releasesPath }, args) {
	return spawnSync(process.execPath, [releaseNotesScript, "fix-github-releases", ...args], {
		encoding: "utf8",
		env: {
			...process.env,
			PATH: `${binDirectory}:${process.env.PATH}`,
			GH_CALL_LOG: callLogPath,
			GH_RELEASES_FILE: releasesPath,
		},
	});
}

function readCalls(callLogPath) {
	return readFileSync(callLogPath, "utf8")
		.trim()
		.split("\n")
		.filter(Boolean)
		.map((line) => JSON.parse(line));
}

const FIXTURE_BODY = [
	"Release notes",
	"",
	"- Fixed the restart loop ([guide](../README.md))",
	"- Documented the RPC mode ([docs](../docs/))",
	"- Imported an upstream fix ([earendil-works/pi#123](https://github.com/earendil-works/pi/issues/123))",
	"- Kept a legacy link ([old repo](https://github.com/badlogic/pi-mono/issues/456))",
].join("\n");

function writeReleases(releasesPath, releases) {
	writeFileSync(
		releasesPath,
		releases.map((release) => JSON.stringify(release)).join("\n") + "\n",
	);
}

test("defaults to LiuXD1011/agent-core, rewrites relative links, preserves upstream history, dry-run writes nothing remotely", () => {
	const workspace = makeWorkspace();
	try {
		writeReleases(workspace.releasesPath, [
			{ id: 2, tag_name: "v0.1.0-alpha.2", body: FIXTURE_BODY },
			{ id: 1, tag_name: "v0.1.0-alpha.1", body: "Older [notes](../README.md)" },
		]);

		const result = runFixReleases(workspace, ["--dry-run"]);
		assert.equal(result.status, 0, result.stderr);

		const calls = readCalls(workspace.callLogPath);
		assert.deepEqual(calls, [["api", "repos/LiuXD1011/agent-core/releases", "--paginate", "--jq", ".[] | {id, tag_name, body} | @json"]]);

		const output = result.stdout;
		assert.match(output, /Would update 2 releases\./);
		assert.match(output, /-> https:\/\/github\.com\/LiuXD1011\/agent-core\/blob\/v0\.1\.0-alpha\.2\/packages\/README\.md/);
		assert.match(output, /-> https:\/\/github\.com\/LiuXD1011\/agent-core\/tree\/v0\.1\.0-alpha\.2\/packages\/docs\//);
		// Retargeting upstream history links would have produced diff lines below; none may appear.
		assert.doesNotMatch(output, /LiuXD1011\/agent-core\/issues\/123/);
		assert.doesNotMatch(output, /LiuXD1011\/agent-core\/issues\/456/);
	} finally {
		workspace.cleanup();
	}
});

test("upstream-only release bodies are left completely untouched", () => {
	const workspace = makeWorkspace();
	try {
		const upstreamBody = [
			"- Upstream fix ([earendil-works/pi#123](https://github.com/earendil-works/pi/issues/123))",
			"- Legacy link ([badlogic#456](https://github.com/badlogic/pi-mono/issues/456))",
			"- Upstream blob ([file](https://github.com/earendil-works/pi/blob/main/packages/coding-agent/README.md))",
		].join("\n");
		writeReleases(workspace.releasesPath, [{ id: 1, tag_name: "v0.1.0-alpha.1", body: upstreamBody }]);

		const result = runFixReleases(workspace, ["--dry-run"]);
		assert.equal(result.status, 0, result.stderr);

		assert.match(result.stdout, /Would update 0 releases\./);
		const calls = readCalls(workspace.callLogPath);
		assert.equal(calls.length, 1);
	} finally {
		workspace.cleanup();
	}
});

test("extract keeps upstream history links verbatim and resolves relative links to Agent Core", () => {
	const workspace = makeWorkspace();
	try {
		const changelogPath = join(workspace.root, "CHANGELOG.md");
		writeFileSync(
			changelogPath,
			`# Changelog\n\n## [0.1.0-alpha.1] - 2026-09-12\n\n- Fixed the restart loop ([guide](../README.md))\n- Imported an upstream fix ([earendil-works/pi#123](https://github.com/earendil-works/pi/issues/123))\n`,
		);

		const result = spawnSync(
			process.execPath,
			[releaseNotesScript, "extract", "--version", "0.1.0-alpha.1", "--tag", "v0.1.0-alpha.1", "--changelog", changelogPath],
			{ encoding: "utf8" },
		);
		assert.equal(result.status, 0, result.stderr);

		assert.match(result.stdout, /https:\/\/github\.com\/LiuXD1011\/agent-core\/blob\/v0\.1\.0-alpha\.1\/packages\/README\.md/);
		assert.match(result.stdout, /https:\/\/github\.com\/earendil-works\/pi\/issues\/123/);
		assert.doesNotMatch(result.stdout, /LiuXD1011\/agent-core\/issues\/123/);
	} finally {
		workspace.cleanup();
	}
});

test("honors an explicit --repo", () => {
	const workspace = makeWorkspace();
	try {
		writeReleases(workspace.releasesPath, [{ id: 1, tag_name: "v0.1.0", body: "[notes](../README.md)" }]);

		const result = runFixReleases(workspace, ["--repo", "example/agent-core-fork", "--dry-run"]);
		assert.equal(result.status, 0, result.stderr);

		const calls = readCalls(workspace.callLogPath);
		assert.equal(calls[0][1], "repos/example/agent-core-fork/releases");
		assert.match(result.stdout, /example\/agent-core-fork\/blob\/v0\.1\.0\/packages\/README\.md/);
	} finally {
		workspace.cleanup();
	}
});

test("--tag patches only that release", () => {
	const workspace = makeWorkspace();
	try {
		writeReleases(workspace.releasesPath, [
			{ id: 2, tag_name: "v0.1.0-alpha.2", body: "[notes](../README.md)" },
			{ id: 1, tag_name: "v0.1.0-alpha.1", body: "[notes](../README.md)" },
		]);

		const result = runFixReleases(workspace, ["--tag", "v0.1.0-alpha.1", "--dry-run"]);
		assert.equal(result.status, 0, result.stderr);

		assert.match(result.stdout, /Would update v0\.1\.0-alpha\.1/);
		assert.doesNotMatch(result.stdout, /v0\.1\.0-alpha\.2/);
		assert.match(result.stdout, /Would update 1 release\./);
	} finally {
		workspace.cleanup();
	}
});

test("--since-tag scopes the release range with prerelease ordering", () => {
	const workspace = makeWorkspace();
	try {
		writeReleases(workspace.releasesPath, [
			{ id: 3, tag_name: "v0.1.0-alpha.2", body: "[notes](../README.md)" },
			{ id: 2, tag_name: "v0.1.0-alpha.1", body: "[notes](../README.md)" },
			{ id: 1, tag_name: "v0.0.9", body: "[notes](../README.md)" },
		]);

		const withinPrerelease = runFixReleases(workspace, ["--since-tag", "v0.1.0-alpha.2", "--dry-run"]);
		assert.equal(withinPrerelease.status, 0, withinPrerelease.stderr);
		assert.match(withinPrerelease.stdout, /Would update 1 release\./);
		assert.match(withinPrerelease.stdout, /v0\.1\.0-alpha\.2/);
		assert.doesNotMatch(withinPrerelease.stdout, /v0\.1\.0-alpha\.1/);
		assert.doesNotMatch(withinPrerelease.stdout, /v0\.0\.9/);

		const stableFloor = runFixReleases(workspace, ["--since-tag", "v0.1.0", "--dry-run"]);
		assert.equal(stableFloor.status, 0, stableFloor.stderr);
		assert.match(stableFloor.stdout, /Would update 0 releases\./);
	} finally {
		workspace.cleanup();
	}
});

test("without --dry-run edits releases in the selected repository through the gh shim", () => {
	const workspace = makeWorkspace();
	try {
		writeReleases(workspace.releasesPath, [{ id: 1, tag_name: "v0.1.0-alpha.1", body: "[notes](../README.md)" }]);

		const result = runFixReleases(workspace, []);
		assert.equal(result.status, 0, result.stderr);

		const editCall = readCalls(workspace.callLogPath).find((call) => call[0] === "release");
		assert.ok(editCall, "expected a gh release edit call");
		assert.equal(editCall[1], "edit");
		assert.equal(editCall[2], "v0.1.0-alpha.1");
		assert.equal(editCall[3], "--repo");
		assert.equal(editCall[4], "LiuXD1011/agent-core");
		assert.equal(editCall[5], "--notes-file");
		assert.match(editCall[6], /\.md$/);
		assert.match(result.stdout, /Updated 1 release\./);
	} finally {
		workspace.cleanup();
	}
});
