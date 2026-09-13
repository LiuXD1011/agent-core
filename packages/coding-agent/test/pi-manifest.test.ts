import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { readPiManifest } from "../src/core/pi-manifest.ts";

function writePackageJson(content: string): string {
	const dir = mkdtempSync(join(tmpdir(), "pi-manifest-"));
	const filePath = join(dir, "package.json");
	writeFileSync(filePath, content, "utf8");
	return filePath;
}

describe("readPiManifest", () => {
	it("reads resource lists from the pi manifest key", () => {
		const filePath = writePackageJson(
			JSON.stringify({
				name: "my-package",
				pi: {
					extensions: ["./custom/extensions"],
					skills: ["./custom/skills"],
					prompts: ["./custom/prompts"],
					themes: ["./custom/themes"],
				},
			}),
		);
		try {
			expect(readPiManifest(filePath)).toEqual({
				extensions: ["./custom/extensions"],
				skills: ["./custom/skills"],
				prompts: ["./custom/prompts"],
				themes: ["./custom/themes"],
			});
		} finally {
			rmSync(join(filePath, ".."), { recursive: true, force: true });
		}
	});

	it("does not read the display-name key; the pi key is the resource protocol", () => {
		// Documented minimal-route behavior: Agent Core keeps the upstream `pi`
		// manifest key; a renamed key is treated as no manifest.
		const filePath = writePackageJson(
			JSON.stringify({
				name: "my-package",
				"agent-core": { extensions: ["./custom/extensions"] },
			}),
		);
		try {
			expect(readPiManifest(filePath)).toBeNull();
		} finally {
			rmSync(join(filePath, ".."), { recursive: true, force: true });
		}
	});

	it("tolerates a UTF-8 BOM", () => {
		const filePath = writePackageJson(`\uFEFF${JSON.stringify({ pi: { extensions: ["./ext"] } })}`);
		try {
			expect(readPiManifest(filePath)).toEqual({ extensions: ["./ext"] });
		} finally {
			rmSync(join(filePath, ".."), { recursive: true, force: true });
		}
	});

	it("rejects non-string and non-array field values per field", () => {
		const filePath = writePackageJson(
			JSON.stringify({
				pi: {
					extensions: ["./ext", 42],
					skills: "not-an-array",
					prompts: [{ invalid: true }],
					themes: ["./themes"],
				},
			}),
		);
		try {
			expect(readPiManifest(filePath)).toEqual({ themes: ["./themes"] });
		} finally {
			rmSync(join(filePath, ".."), { recursive: true, force: true });
		}
	});

	it("returns null for invalid JSON, non-objects, and manifests without a pi key", () => {
		const invalid = writePackageJson("{not json");
		const arrayRoot = writePackageJson("[]");
		const noManifest = writePackageJson(JSON.stringify({ name: "my-package" }));
		try {
			expect(readPiManifest(invalid)).toBeNull();
			expect(readPiManifest(arrayRoot)).toBeNull();
			expect(readPiManifest(noManifest)).toBeNull();
		} finally {
			for (const filePath of [invalid, arrayRoot, noManifest]) {
				rmSync(join(filePath, ".."), { recursive: true, force: true });
			}
		}
	});
});
