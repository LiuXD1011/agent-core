import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { chmodSync, cpSync, existsSync, mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const repoRoot = fileURLToPath(new URL("..", import.meta.url));
const buildScript = join(repoRoot, "scripts", "build-binaries.sh");

const PLATFORMS = ["darwin-arm64", "darwin-x64", "linux-x64", "linux-arm64", "windows-x64", "windows-arm64"];

function makeShims(binDirectory, logDirectory) {
	const logFile = join(logDirectory, "tool-calls.log");

	// Generic shim template: append args to the log, then emulate the tool
	// effects the build script relies on (created outputs).
	const bunShim = join(binDirectory, "bun");
	writeFileSync(
		bunShim,
		`#!/usr/bin/env bash
printf 'bun %s\\n' "$*" >> ${JSON.stringify(logFile)}
prev=""
for arg in "$@"; do
  if [[ "$prev" == "--outfile" ]]; then
    mkdir -p "$(dirname "$arg")"
    : > "$arg"
  fi
  prev="$arg"
done
`,
	);
	chmodSync(bunShim, 0o755);

	const tarShim = join(binDirectory, "tar");
	writeFileSync(
		tarShim,
		`#!/usr/bin/env bash
printf 'tar %s\\n' "$*" >> ${JSON.stringify(logFile)}
creating=false
archive=""
for arg in "$@"; do
  case "$arg" in -c*) creating=true ;; esac
  case "$arg" in *.tar.gz) archive="$arg" ;; esac
done
if [[ "$creating" == "true" && -n "$archive" ]]; then
  : > "$archive"
else
  # Emulate extraction: the wrapper directory is the archive root.
  mkdir -p agent-core
fi
`,
	);
	chmodSync(tarShim, 0o755);

	// The test asserts tool invocation names only; copies of repo assets are
	// irrelevant and can fail on cross-platform native files that --skip-deps
	// does not install.
	const cpShim = join(binDirectory, "cp");
	writeFileSync(
		cpShim,
		`#!/usr/bin/env bash
printf 'cp %s\\n' "$*" >> ${JSON.stringify(logFile)}
exit 0
`,
	);
	chmodSync(cpShim, 0o755);

	const zipShim = join(binDirectory, "zip");
	writeFileSync(
		zipShim,
		`#!/usr/bin/env bash
printf 'zip %s\\n' "$*" >> ${JSON.stringify(logFile)}
prev=""
for arg in "$@"; do
  if [[ "$prev" == "-r" ]]; then : > "$arg"; fi
  prev="$arg"
done
`,
	);
	chmodSync(zipShim, 0o755);

	const unzipShim = join(binDirectory, "unzip");
	writeFileSync(
		unzipShim,
		`#!/usr/bin/env bash
printf 'unzip %s\\n' "$*" >> ${JSON.stringify(logFile)}
`,
	);
	chmodSync(unzipShim, 0o755);
}

test("build-binaries.sh emits agent-core binaries and agent-core archives for all six platforms", () => {
	const root = mkdtempSync(join(tmpdir(), "pi-binary-branding-"));
	const binDirectory = join(root, "bin");
	const logDirectory = join(root, "logs");
	const outDirectory = join(root, "binaries");
	mkdirSync(binDirectory);
	mkdirSync(logDirectory);
	const logFile = join(logDirectory, "tool-calls.log");
	makeShims(binDirectory, logDirectory);

	try {
		const result = spawnSync("bash", [buildScript, "--skip-install", "--skip-deps", "--skip-build", "--out", outDirectory], {
			cwd: repoRoot,
			encoding: "utf8",
			env: { ...process.env, PATH: `${binDirectory}:${process.env.PATH}` },
			timeout: 120_000,
		});
		assert.equal(result.status, 0, `build script failed: ${result.stderr}\n${result.stdout}`);

		const calls = readFileSync(logFile, "utf8").trim().split("\n");

		// Bun compiles exactly one agent-core binary per platform directory.
		const bunOutfiles = calls
			.filter((line) => line.startsWith("bun "))
			.map((line) => {
				const match = /--outfile (\S+)$/.exec(line);
				return match?.[1];
			})
			.filter(Boolean);
		assert.equal(bunOutfiles.length, 6);
		for (const platform of PLATFORMS) {
			const expectedBinary = platform.startsWith("windows-") ? "agent-core.exe" : "agent-core";
			assert.ok(
				bunOutfiles.some((outfile) => outfile === join(outDirectory, platform, expectedBinary)),
				`missing bun outfile for ${platform}: ${bunOutfiles.join(", ")}`,
			);
		}
		for (const outfile of bunOutfiles) {
			assert.equal(outfile.endsWith("/agent-core") || outfile.endsWith("/agent-core.exe"), true, `unexpected binary name: ${outfile}`);
			assert.doesNotMatch(outfile, /\/pi(\.exe)?$/, `bare pi binary must not be produced: ${outfile}`);
		}

		// Binary runtime resolves HTML templates next to the executable as export-html/.
		for (const platform of PLATFORMS) {
			assert.ok(
				calls.includes(`cp -r dist/session/export/html ${join(outDirectory, platform, "export-html")}`),
				`missing binary HTML export assets for ${platform}`,
			);
		}

		// Unix archives wrap a agent-core directory; every archive is agent-core-<platform>.
		const tarLines = calls.filter((line) => line.startsWith("tar ") && line.includes("-czf"));
		assert.equal(tarLines.length, 4);
		for (const platform of PLATFORMS.filter((entry) => !entry.startsWith("windows-"))) {
			assert.ok(
				tarLines.some((line) => line.includes(`agent-core-${platform}.tar.gz`) && line.includes(" agent-core")),
				`missing agent-core tar wrapper for ${platform}`,
			);
		}

		const zipLines = calls.filter((line) => line.startsWith("zip "));
		assert.equal(zipLines.length, 2);
		for (const platform of PLATFORMS.filter((entry) => entry.startsWith("windows-"))) {
			assert.ok(
				zipLines.some((line) => line.includes(`../agent-core-${platform}.zip`)),
				`missing agent-core zip for ${platform}`,
			);
		}

		// No bare pi archives on disk.
		for (const entry of [...PLATFORMS.map((platform) => `pi-${platform}.tar.gz`), ...PLATFORMS.map((platform) => `pi-${platform}.zip`)]) {
			assert.equal(existsSync(join(outDirectory, entry)), false, `unexpected legacy archive: ${entry}`);
		}
		for (const platform of PLATFORMS.filter((entry) => !entry.startsWith("windows-"))) {
			assert.ok(existsSync(join(outDirectory, `agent-core-${platform}.tar.gz`)), `missing archive for ${platform}`);
		}
		for (const platform of PLATFORMS.filter((entry) => entry.startsWith("windows-"))) {
			assert.ok(existsSync(join(outDirectory, `agent-core-${platform}.zip`)), `missing archive for ${platform}`);
		}
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});

test("local-release.mjs shims and messages reference only agent-core", async () => {
	const { readFile } = await import("node:fs/promises");
	const source = await readFile(join(repoRoot, "scripts", "local-release.mjs"), "utf8");
	assert.match(source, /agent-core-\$\{platform\}\.(zip|tar\.gz)/);
	assert.doesNotMatch(source, /node_modules", "\.bin", "pi"\)/);
	assert.doesNotMatch(source, /"pi\.exe"/);
	assert.doesNotMatch(source, /"pi\.cmd"/);

	const consumer = await readFile(join(repoRoot, "scripts", "coding-agent-consumer.mjs"), "utf8");
	assert.match(consumer, /manifest\.bin\["agent-core"\]/);
	assert.doesNotMatch(consumer, /manifest\.bin\.pi\b/);
});


test("copy-assets succeeds without the removed announcement image directory", () => {
	const root = mkdtempSync(join(tmpdir(), "agent-core-copy-assets-"));
	const packageRoot = join(repoRoot, "packages", "agent-app");
	const manifest = JSON.parse(readFileSync(join(packageRoot, "package.json"), "utf8"));
	try {
		for (const directory of ["src/ui/terminal/theme", "src/session/export/html"]) {
			cpSync(join(packageRoot, directory), join(root, directory), { recursive: true });
		}
		const result = spawnSync("bash", ["-c", manifest.scripts["copy-assets"]], {
			cwd: root,
			encoding: "utf8",
			env: { ...process.env, PATH: `${join(repoRoot, "node_modules", ".bin")}:${process.env.PATH}` },
			timeout: 30_000,
		});
		assert.equal(result.status, 0, `${result.stderr}\n${result.stdout}`);
		assert.ok(existsSync(join(root, "dist/ui/terminal/theme/dark.json")));
		assert.ok(existsSync(join(root, "dist/session/export/html/template.html")));
		assert.equal(existsSync(join(root, "dist/ui/terminal/assets")), false);
		assert.doesNotMatch(manifest.scripts["copy-binary-assets"], /src\/modes\/interactive\/assets/);
		assert.doesNotMatch(readFileSync(buildScript, "utf8"), /dist\/modes\/interactive\/assets/);
	} finally {
		rmSync(root, { recursive: true, force: true });
	}
});
