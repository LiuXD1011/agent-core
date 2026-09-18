import { spawnSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, test } from "vitest";

const packageDir = fileURLToPath(new URL("../", import.meta.url));
const configUrl = new URL("../src/config.ts", import.meta.url).href;

function readIdentity(directory: string) {
	const result = spawnSync(
		process.execPath,
		[
			"--import",
			"tsx",
			"--input-type=module",
			"-e",
			"import { APP_NAME, APP_TITLE, PACKAGE_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR } from " +
				JSON.stringify(configUrl) +
				"; console.log(JSON.stringify({ APP_NAME, APP_TITLE, PACKAGE_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR }));",
		],
		{
			cwd: packageDir,
			env: { ...process.env, AGENT_CORE_PACKAGE_DIR: directory },
			encoding: "utf8",
			timeout: 10000,
		},
	);
	expect(result.error).toBeUndefined();
	expect(result.status, result.stderr).toBe(0);
	return JSON.parse(result.stdout) as Record<string, string>;
}

describe("application identity manifest", () => {
	test("keeps the shipped Agent Core identity and config paths unchanged", () => {
		const manifest = JSON.parse(readFileSync(join(packageDir, "package.json"), "utf8")) as Record<string, unknown>;
		expect(manifest).not.toHaveProperty("piConfig");
		expect(manifest.agentCoreConfig).toEqual({ configDir: ".agent-core" });
		expect(readIdentity(packageDir)).toEqual({
			APP_NAME: "agent-core",
			APP_TITLE: "Agent Core",
			PACKAGE_NAME: "@liuxuedeng/agent-core",
			CONFIG_DIR_NAME: ".agent-core",
			ENV_AGENT_DIR: "AGENT_CORE_CODING_AGENT_DIR",
		});
	});

	test("reads custom identity from agentCoreConfig", () => {
		const directory = mkdtempSync(join(tmpdir(), "agent-core-identity-"));
		try {
			writeFileSync(
				join(directory, "package.json"),
				JSON.stringify({
					name: "@example/demo-agent",
					agentCoreConfig: { name: "demo-agent", configDir: ".demo-agent" },
				}),
			);
			expect(readIdentity(directory)).toEqual({
				APP_NAME: "demo-agent",
				APP_TITLE: "demo-agent",
				PACKAGE_NAME: "@example/demo-agent",
				CONFIG_DIR_NAME: ".demo-agent",
				ENV_AGENT_DIR: "DEMO_AGENT_CODING_AGENT_DIR",
			});
		} finally {
			rmSync(directory, { recursive: true, force: true });
		}
	});
});
