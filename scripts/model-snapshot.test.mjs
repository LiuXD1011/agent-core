import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { readdirSync, readFileSync } from "node:fs";
import test from "node:test";

test("ordinary builds use the pinned model snapshot; updates remain explicit", () => {
	const root = JSON.parse(readFileSync("package.json", "utf8"));
	const ai = JSON.parse(readFileSync("packages/ai/package.json", "utf8"));
	assert.equal(ai.scripts.build, "npm run build:offline");
	assert.match(ai.scripts["build:offline"], /check:model-data/);
	assert.doesNotMatch(root.scripts.build, /generate-models|hydrate:model-data/);
	assert.match(root.scripts["generate:models"], /generate-models/);
	assert.match(ai.scripts["generate-models"], /generate-models\.ts --strict/);
});

test("model values and their integrity manifest are complete and not ignored", () => {
	const directory = "packages/ai/src/providers/data";
	const files = readdirSync(directory).filter((file) => file.endsWith(".json"));
	assert.ok(files.includes(".manifest.json"), "snapshot integrity manifest missing");
	assert.ok(files.length > 1, "provider snapshots missing");
	const result = spawnSync("git", ["check-ignore", "--no-index", "--stdin"], {
		input: files.map((file) => directory + "/" + file).join("\n") + "\n",
		encoding: "utf8",
	});
	assert.equal(result.status, 1, result.stdout + result.stderr);
	assert.equal(result.stdout, "");
	const validation = spawnSync(process.execPath, ["packages/ai/scripts/check-model-data.ts"], { encoding: "utf8" });
	assert.equal(validation.status, 0, validation.stdout + validation.stderr);
});

test("CI runs the isolated non-e2e test entry point", () => {
	const workflow = readFileSync(".github/workflows/ci.yml", "utf8");
	assert.match(workflow, /run: bash \.\/test\.sh/);
	assert.doesNotMatch(workflow, /run: npm test/);
});
