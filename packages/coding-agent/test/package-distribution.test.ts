import { readFileSync } from "node:fs";
import { describe, expect, test } from "vitest";

interface CodingAgentPackageJson {
	bin: { "agent-core": string };
	main: string;
	exports: {
		".": { import: string; types: string };
		"./rpc-entry": { import: string };
	};
}

const packageJson = JSON.parse(
	readFileSync(new URL("../package.json", import.meta.url), "utf8"),
) as CodingAgentPackageJson;

describe("package distribution entrypoints", () => {
	test("uses the bundle for executables and modular output for libraries", () => {
		expect(packageJson.bin["agent-core"]).toBe("dist/bundle/cli.js");
		expect(packageJson.main).toBe("./dist/index.js");
		expect(packageJson.exports["."].import).toBe("./dist/index.js");
		expect(packageJson.exports["./rpc-entry"].import).toBe("./dist/bundle/rpc-entry.js");
	});

	// Regression for #9132: internal experimental entrypoints must not be published runtime exports.
	test("keeps experimental and remote-chain entrypoints out of the exports map", () => {
		const exportsMap = packageJson.exports as Record<string, unknown>;
		expect("./client" in exportsMap).toBe(false);
		expect("./experimental/plugin" in exportsMap).toBe(false);
	});
});
