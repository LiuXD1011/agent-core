import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
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
		(directory) => !directory.includes("/examples/") && !directory.includes("/docs/") && directory !== "packages/coding-agent/install-lock",
	);
	assert.equal(directories.length, 8, `expected the full workspace set, found ${directories.length}`);

	for (const directory of directories) {
		const pkg = JSON.parse(readFileSync(`${directory}/package.json`, "utf8"));

		assert.match(pkg.name, /^@liuxuedeng\/agent-core(-|$)/, `${directory}: unexpected package name`);
		assert.equal(pkg.version, LOCKSTEP_VERSION, `${directory}: lockstep version mismatch`);

		assert.equal(pkg.repository?.type, "git", `${directory}: repository type`);
		assert.equal(pkg.repository?.url, REPO_URL, `${directory}: repository must point at LiuXD1011/agent-core`);
		assert.equal(pkg.repository?.directory, directory, `${directory}: repository directory mismatch`);

		assert.equal(pkg.author, "LiuXD1011", `${directory}: author`);
		assert.ok(
			(pkg.contributors ?? []).some((contributor) => /original Pi/.test(contributor)),
			`${directory}: original author credit missing`,
		);
		assert.equal(pkg.homepage, "https://github.com/LiuXD1011/agent-core#readme", `${directory}: homepage`);
		assert.equal(pkg.bugs?.url, "https://github.com/LiuXD1011/agent-core/issues", `${directory}: bugs`);
	}
});

test("root name check in local-release.mjs matches the current root package name", async () => {
	const { readFile } = await import("node:fs/promises");
	const source = await readFile("scripts/local-release.mjs", "utf8");
	assert.match(source, new RegExp(`rootPackageJson\\.name !== "${ROOT_NAME}"`));
	assert.doesNotMatch(source, /pi-monorepo/);
});
