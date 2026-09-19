import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = fileURLToPath(new URL("..", import.meta.url));

test("ignore root docs, local credentials and generated output without hiding source or package docs", () => {
	const ignored = [
		"node_modules/example/index.js", "packages/agent-app/dist/cli.js", ".env", ".env.production",
		"packages/agent-app/.env.local", ".agent-core/auth.json", ".agent-core/agent/auth.json",
		".agent-core/sessions/run.jsonl", ".agent-core/context/session/output.txt", ".agent-core/bin/rg",
		"packages/agent-app/agent-core-session-test.html", "session-2026-09-16.jsonl",
		"packages/agent-app/session-2026-09-16.jsonl", "session-transcripts/run.txt",
		"docs/architecture/packages.visual-check.receipt.json", "coverage/index.html",
		"docs/README.md", "docs/architecture/index.html", "docs/architecture/packages.html", "docs/new/nested.md",
	];
	const retained = [
		"README.md", "packages/agent-app/docs/usage.md", "packages/ai/docs/new.md", ".env.example",
		"packages/agent-app/.env.template", ".agent-core/context.json", ".agent-core/extensions/tps.ts",
		"packages/agent-app/test/fixtures/session-sample.jsonl", "agent-core-reference.html",
		"scripts/session-transcripts.ts", "todo.md", "plans/design.md", "collect.sh",
	];
	const result = spawnSync("git", ["-c", "core.excludesFile=/dev/null", "check-ignore", "--no-index", "--stdin", "-z"], {
		cwd: root, input: [...ignored, ...retained].join("\0") + "\0", encoding: "utf8",
	});
	assert.equal(result.status, 0, result.stderr);
	const actual = new Set(result.stdout.split("\0").filter(Boolean));
	for (const path of ignored) assert.ok(actual.has(path), `must ignore ${path}`);
	for (const path of retained) assert.ok(!actual.has(path), `must retain ${path}`);
});

test("session transcript output defaults to the ignored directory and honors an explicit directory", () => {
	const directory = mkdtempSync(join(tmpdir(), "agent-core-transcripts-"));
	try {
		const home = join(directory, "home");
		const project = join(directory, "project");
		mkdirSync(project);
		const sessionDirectory = join(home, ".agent-core", "agent", "sessions", `--${project.replaceAll("/", "-").slice(1)}--`);
		mkdirSync(sessionDirectory, { recursive: true });
		writeFileSync(join(sessionDirectory, "sample.jsonl"), [
			{ type: "session", version: 3, id: "test", cwd: project, timestamp: "2026-09-16T00:00:00.000Z" },
			{ type: "message", id: "first", parentId: null, timestamp: "2026-09-16T00:00:00.000Z", message: { role: "user", content: "offline transcript", timestamp: 1 } },
		].map((entry) => JSON.stringify(entry)).join("\n") + "\n");
		for (const output of [undefined, "custom-output"]) {
			const args = [join(root, "scripts/session-transcripts.ts"), ...(output ? ["--output", output] : [])];
			const result = spawnSync(process.execPath, args, {
				cwd: project, encoding: "utf8", timeout: 30_000,
				env: { PATH: process.env.PATH, HOME: home, USERPROFILE: home },
			});
			assert.equal(result.status, 0, `${result.stderr}\n${result.stdout}`);
			assert.match(readFileSync(join(project, output ?? "session-transcripts", "session-transcripts-000.txt"), "utf8"), /offline transcript/);
		}
		assert.equal(existsSync(join(project, "session-transcripts.ts")), false);
	} finally {
		rmSync(directory, { recursive: true, force: true });
	}
});