import assert from "node:assert/strict";
import { existsSync, readFileSync } from "node:fs";
import test from "node:test";
import { findPackageDirectories } from "./package-workspaces.mjs";

const REPO_URL = "git+https://github.com/LiuXD1011/agent-core.git";
const LOCKSTEP_VERSION = "0.1.0-alpha.1";
const ROOT_NAME = "agent-core-monorepo";

test("workspace metadata is unified on the Agent Core repository", () => {
	const root = JSON.parse(readFileSync("package.json", "utf8"));
	assert.equal(root.name, ROOT_NAME);
	assert.equal(root.version, LOCKSTEP_VERSION, "root version must match the workspace lockstep");

	// Example extensions, doc fixtures, and the generated install-lock are not brand-bearing packages.
	const directories = findPackageDirectories().filter(
		(directory) => !directory.includes("/examples/") && !directory.includes("/docs/") && directory !== "packages/agent-app/install-lock",
	);
	assert.equal(directories.length, 4, `expected the full workspace set, found ${directories.length}`);

	for (const directory of directories) {
		const pkg = JSON.parse(readFileSync(`${directory}/package.json`, "utf8"));

		assert.match(pkg.name, /^@liuxuedeng\/agent-core(-|$)/, `${directory}: unexpected package name`);
		assert.equal(pkg.version, LOCKSTEP_VERSION, `${directory}: lockstep version mismatch`);

		assert.equal(pkg.repository?.type, "git", `${directory}: repository type`);
		assert.equal(pkg.repository?.url, REPO_URL, `${directory}: repository must point at LiuXD1011/agent-core`);
		assert.equal(pkg.repository?.directory, directory, `${directory}: repository directory mismatch`);

		assert.equal(pkg.author, "LiuXD1011", `${directory}: author`);
		assert.ok(
			(pkg.contributors ?? []).includes("Mario Zechner"),
			`${directory}: original author credit missing`,
		);
		assert.equal(pkg.homepage, "https://github.com/LiuXD1011/agent-core#readme", `${directory}: homepage`);
		assert.equal(pkg.bugs?.url, "https://github.com/LiuXD1011/agent-core/issues", `${directory}: bugs`);
	}
});

test("root name check in local-release.mjs matches the current root package name", () => {
	const source = readFileSync("scripts/local-release.mjs", "utf8");
	assert.match(source, new RegExp(`rootPackageJson\\.name !== "${ROOT_NAME}"`));
	assert.doesNotMatch(source, /pi-monorepo/);
});


test("explicit workspaces and the root lockfile match retained packages", () => {
	const root = JSON.parse(readFileSync("package.json", "utf8"));
	const lock = JSON.parse(readFileSync("package-lock.json", "utf8"));
	assert.deepEqual(lock.packages[""].workspaces, root.workspaces);
	for (const directory of root.workspaces.filter((entry) => !entry.includes("*"))) {
		assert.ok(existsSync(`${directory}/package.json`), `missing workspace: ${directory}`);
	}
	for (const directory of Object.keys(lock.packages)) {
		if (directory.startsWith("packages/") && !directory.includes("/node_modules/")) {
			assert.ok(existsSync(`${directory}/package.json`), `obsolete workspace metadata: ${directory}`);
		}
	}
	for (const entry of Object.values(lock.packages)) {
		if (entry.link && entry.resolved.startsWith("packages/")) {
			assert.ok(existsSync(`${entry.resolved}/package.json`), `dangling workspace link: ${entry.resolved}`);
		}
	}
});


test("the retired evaluation workspace is absent from scripts and dependency metadata", () => {
	const root = JSON.parse(readFileSync("package.json", "utf8"));
	const lock = JSON.parse(readFileSync("package-lock.json", "utf8"));
	assert.equal(existsSync("benchmarks"), false);
	assert.equal(root.scripts.eval, undefined);
	assert.equal(root.workspaces.some((path) => path.startsWith("benchmarks/")), false);
	for (const [path, metadata] of Object.entries(lock.packages)) {
		assert.equal(path.startsWith("benchmarks/"), false, path);
		assert.notEqual(metadata.resolved, "benchmarks/evals", path);
		assert.equal(path.endsWith("node_modules/vitest-evals"), false, path);
	}
});
