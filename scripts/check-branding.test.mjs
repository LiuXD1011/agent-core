import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const checker = fileURLToPath(new URL("./check-branding.mjs", import.meta.url));

function makeFixture(files) {
	const root = mkdtempSync(join(tmpdir(), "agent-core-check-branding-"));
	for (const [relativePath, content] of Object.entries(files)) {
		const fullPath = join(root, relativePath);
		mkdirSync(dirname(fullPath), { recursive: true });
		writeFileSync(fullPath, content);
	}
	return root;
}

const CORRECT_PACKAGE_JSON = JSON.stringify({
	name: "@liuxuedeng/agent-core",
	version: "0.1.0-alpha.1",
	author: "LiuXD1011",
	repository: { type: "git", url: "git+https://github.com/LiuXD1011/agent-core.git", directory: "packages/agent-app" },
});

const CORRECT_SETUP_TS = `import { APP_NAME } from "../config.ts";
process.env.AI_AGENT = APP_NAME;
`;

const CORRECT_VERSION_CHECK_TS = `// Agent Core does not track the upstream pi.dev release feed: following it would
// misreport upstream Pi versions as Agent Core updates.
export function resolveVersionCheckUrl() {
	return process.env.AGENT_CORE_VERSION_CHECK_URL;
}
`;

const CORRECT_FILES = {
	"NOTICE.md": "部分源码源自 [Pi](https://github.com/earendil-works/pi) v0.85.1，MIT 许可。",
	"README.md": [
		"# Agent Core",
		"",
		"- 感谢 [Pi](https://github.com/earendil-works/pi) 的作者与贡献者提供的基础代码。",
		"",
		"```bash",
		"npm install -g --ignore-scripts @liuxuedeng/agent-core",
		"```",
	].join("\n"),
	"CHANGELOG.md": [
		"# Changelog",
		"",
		"## [Unreleased]",
		"",
		"- Agent Core changes.",
		"",
		"<!-- agent-core:upstream-boundary -->",
		"",
		"## [0.85.1] - 2026-09-05",
		"",
		"- Upstream fix ([#9166](https://github.com/earendil-works/pi/pull/9166)).",
	].join("\n"),
	"packages/agent-app/package.json": CORRECT_PACKAGE_JSON,
	"packages/agent-app/src/cli/setup.ts": CORRECT_SETUP_TS,
	"packages/agent-app/src/utils/version-check.ts": CORRECT_VERSION_CHECK_TS,
	"scripts/build-binaries.sh": 'bun build --compile --outfile "$OUTPUT_DIR/$platform/agent-core"\n',
};

function runChecker(root) {
	return spawnSync(process.execPath, [checker, root], { encoding: "utf8" });
}

test("correct branding, copyright attribution, and history links pass", () => {
	const root = makeFixture(CORRECT_FILES);
	try {
		const result = runChecker(root);
		assert.equal(result.status, 0, result.stderr);
		assert.match(result.stdout, /Branding check passed/);
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("upstream install commands, wrong repositories, legacy env vars, and bare pi binaries each fail", () => {
	const cases = [
		{
			name: "upstream branding outside the README attribution",
			file: "README.md",
			content: "Agent Core is a secondary development of [Pi](https://github.com/earendil-works/pi).",
		},
		{
			name: "upstream npm install command",
			file: "docs/install.md",
			content: "```bash\nnpm install -g @earendil-works/pi-coding-agent\n```",
		},
		{
			name: "upstream installer script",
			file: "docs/install.md",
			content: "```bash\ncurl -fsSL https://pi.dev/install.sh | sh\n```",
		},
		{
			name: "wrong repository metadata",
			file: "packages/agent-app/package.json",
			content: CORRECT_PACKAGE_JSON.replace("LiuXD1011/agent-core.git", "earendil-works/pi.git"),
		},
		{
			name: "upstream author metadata",
			file: "packages/agent-app/package.json",
			content: CORRECT_PACKAGE_JSON.replace('"author":"LiuXD1011"', '"author":"Mario Zechner"'),
		},
		{
			name: "legacy environment variable example",
			file: "docs/env.md",
			content: "Set `PI_SKIP_VERSION_CHECK=1` to skip the check.",
		},
		{
			name: "current documentation linking upstream services",
			file: "docs/links.md",
			content: "See the changelog at https://pi.dev/changelog.",
		},
		{
			name: "bare pi binary output",
			file: "scripts/build-binaries.sh",
			content: 'bun build --compile --outfile "$OUTPUT_DIR/$platform/pi"\n',
		},
		{
			name: "AI_AGENT hardcoded to pi",
			file: "packages/agent-app/src/cli/setup.ts",
			content: 'process.env.AI_AGENT = "pi";\n',
		},
	];

	for (const { name, file, content } of cases) {
		const root = makeFixture({ ...CORRECT_FILES, [file]: content });
		try {
			const result = runChecker(root);
			assert.equal(result.status, 1, `${name}: expected the checker to fail`);
			assert.match(result.stderr, /Branding check failed/, `${name}: expected a violation report`);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	}
});

test("expanded coverage: self-reference, old commands, schema URLs, UA, auth help, build bypass, archive root, and legacy bins each fail", () => {
	const cases = [
		{
			name: "Pi Core self-reference in current documentation",
			file: "docs/identity.md",
			content: "Pi Core is an independent project.",
		},
		{
			name: "old pi launch command in current documentation",
			file: "docs/run.md",
			content: "```bash\npi -e ./my-extension.ts\n```",
		},
		{
			name: "upstream raw schema URL in a JSON fixture",
			file: "docs/theme-schema.json",
			content: '{\n  "$schema": "https://raw.githubusercontent.com/earendil-works/pi/main/packages/agent-app/src/ui/terminal/theme/theme-schema.json"\n}\n',
		},
		{
			name: "AI user-agent brand regression",
			file: "packages/ai/src/utils/pi-user-agent.ts",
			content: 'export function getPiUserAgent() {\n\treturn `pi (${os.platform()})`;\n}\n',
		},
		{
			name: "auth command help hardcoded to pi",
			file: "packages/agent-app/src/cli/auth-command.ts",
			content: 'console.log("Usage:\\n  pi auth print-api-key");\n',
		},
		{
			name: "build:binary outputs dist/pi",
			file: "packages/agent-app/package.json",
			content: CORRECT_PACKAGE_JSON.replace("{", '{"scripts":{"build:binary":"bun build --compile --outfile dist/pi"},'),
		},
		{
			name: "source archive root stays pi-prefixed",
			file: "scripts/create-source-archive.sh",
			content: 'archive_root="pi-${version}"\n',
		},
		{
			name: "legacy pi binary name reintroduced",
			file: "packages/agent-app/package.json",
			content: CORRECT_PACKAGE_JSON.replace("{", '{"bin":{"pi":"dist/bundle/cli.js"},'),
		},
	];

	for (const { name, file, content } of cases) {
		const root = makeFixture({ ...CORRECT_FILES, [file]: content });
		try {
			const result = runChecker(root);
			assert.equal(result.status, 1, `${name}: expected the checker to fail`);
			assert.match(result.stderr, /Branding check failed/, `${name}: expected a violation report`);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	}
});

test("released changelog history stays protected while the Unreleased section is checked", () => {
	const historyEntry = "- Upstream fix with old commands: `pi -e ./x.ts` ([#9166](https://github.com/earendil-works/pi/pull/9166)).";
	const releasedSection = ["## [0.85.1] - 2026-09-05", "", historyEntry].join("\n");

	const passingRoot = makeFixture({
		...CORRECT_FILES,
		"CHANGELOG.md": ["# Changelog", "", "## [Unreleased]", "", "- Agent Core changes.", "", "<!-- agent-core:upstream-boundary -->", "", releasedSection].join("\n"),
	});
	try {
		const result = runChecker(passingRoot);
		assert.equal(result.status, 0, result.stderr);
	} finally {
		rmSync(passingRoot, { recursive: true, force: true });
	}

	const failingRoot = makeFixture({
		...CORRECT_FILES,
		"CHANGELOG.md": ["# Changelog", "", "## [Unreleased]", "", historyEntry].join("\n"),
	});
	try {
		const result = runChecker(failingRoot);
		assert.equal(result.status, 1, "expected old launch commands in the Unreleased section to fail");
	} finally {
		rmSync(failingRoot, { recursive: true, force: true });
	}
});

test("the checker never writes the files it inspects", () => {
	const root = makeFixture(CORRECT_FILES);
	const hashesBefore = new Map();
	for (const [relativePath, content] of Object.entries(CORRECT_FILES)) {
		hashesBefore.set(relativePath, createHash("sha256").update(content).digest("hex"));
	}
	try {
		runChecker(root);
		for (const [relativePath, hash] of hashesBefore) {
			const current = createHash("sha256").update(readFileSync(join(root, relativePath))).digest("hex");
			assert.equal(current, hash, `${relativePath} was modified by the checker`);
		}
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});
