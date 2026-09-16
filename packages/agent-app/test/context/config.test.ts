/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { DEFAULT_CONFIG, findConfigPath, loadContextConfig } from "../../src/context/config.ts";
import { DEFAULT_REDUCER_MODEL, DEFAULT_REDUCER_PROVIDER } from "../../src/context/reduction/config.ts";
import { CONFIG_DIR_NAME } from "./host.ts";

const roots: string[] = [];

function fixture(): { agentDir: string; cwd: string } {
	const root = mkdtempSync(join(tmpdir(), "context-config-"));
	roots.push(root);
	const cwd = join(root, "project");
	const agentDir = join(root, "agent");
	mkdirSync(cwd, { recursive: true });
	mkdirSync(agentDir, { recursive: true });
	return { agentDir, cwd };
}

afterEach(() => {
	for (const root of roots.splice(0)) rmSync(root, { force: true, recursive: true });
});

describe("Context config", () => {
	it("returns built-in defaults when neither config exists", () => {
		const { agentDir, cwd } = fixture();
		expect(findConfigPath(cwd, agentDir, true)).toBeUndefined();
		expect(loadContextConfig(cwd, agentDir, true)).toEqual(DEFAULT_CONFIG);
		expect(DEFAULT_CONFIG.cacheWriteReadRatio).toBe(12.5);
		expect(DEFAULT_CONFIG.logReductionProvider).toBe(DEFAULT_REDUCER_PROVIDER);
		expect(DEFAULT_CONFIG.logReductionModel).toBe(DEFAULT_REDUCER_MODEL);
	});

	it("loads the global config as a fallback", () => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "context.json");
		writeFileSync(path, JSON.stringify({ version: 1, resultReferences: true, boundaryCompaction: true }));
		expect(findConfigPath(cwd, agentDir, true)).toBe(path);
		expect(loadContextConfig(cwd, agentDir, true)).toEqual({
			...DEFAULT_CONFIG,
			resultReferences: true,
			boundaryCompaction: true,
		});
	});

	it("uses the project config instead of merging the global config", () => {
		const { agentDir, cwd } = fixture();
		writeFileSync(join(agentDir, "context.json"), JSON.stringify({ version: 1, resultReferences: true }));
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		const projectPath = join(cwd, CONFIG_DIR_NAME, "context.json");
		writeFileSync(projectPath, JSON.stringify({ version: 1, mutationCommands: true }));

		expect(findConfigPath(cwd, agentDir, true)).toBe(projectPath);
		expect(loadContextConfig(cwd, agentDir, true)).toEqual({ ...DEFAULT_CONFIG, mutationCommands: true });
	});

	it("ignores the project config when the user has not trusted the project", () => {
		const { agentDir, cwd } = fixture();
		writeFileSync(join(agentDir, "context.json"), JSON.stringify({ version: 1, resultReferences: true }));
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(join(cwd, CONFIG_DIR_NAME, "context.json"), JSON.stringify({ version: 1, mutationCommands: true }));

		expect(loadContextConfig(cwd, agentDir, false)).toEqual({ ...DEFAULT_CONFIG, resultReferences: true });
	});

	it("rejects unknown keys", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(join(cwd, CONFIG_DIR_NAME, "context.json"), JSON.stringify({ version: 1, actionFussion: true }));
		expect(() => loadContextConfig(cwd, agentDir, true)).toThrow("Unknown Context config key: actionFussion");
	});

	it("rejects non-boolean feature values", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(
			join(cwd, CONFIG_DIR_NAME, "context.json"),
			JSON.stringify({ version: 1, mutationCommands: "yes" }),
		);
		expect(() => loadContextConfig(cwd, agentDir, true)).toThrow("Context config mutationCommands must be boolean");
	});

	it("rejects a non-boolean Context compaction value", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(
			join(cwd, CONFIG_DIR_NAME, "context.json"),
			JSON.stringify({ version: 1, boundaryCompaction: "yes" }),
		);
		expect(() => loadContextConfig(cwd, agentDir, true)).toThrow("Context config boundaryCompaction must be boolean");
	});

	it("loads an explicit cache write/read ratio, including zero", () => {
		for (const cacheWriteReadRatio of [0, 3.25]) {
			const { agentDir, cwd } = fixture();
			const path = join(agentDir, "context.json");
			writeFileSync(path, JSON.stringify({ version: 1, cacheWriteReadRatio }));
			expect(loadContextConfig(cwd, agentDir, true).cacheWriteReadRatio).toBe(cacheWriteReadRatio);
		}
	});

	it("loads an explicit Verified log reduction provider/model route", () => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "context.json");
		writeFileSync(
			path,
			JSON.stringify({
				version: 1,
				logReductionProvider: "test-provider",
				logReductionModel: "test-reducer-model",
			}),
		);
		expect(loadContextConfig(cwd, agentDir, true)).toEqual({
			...DEFAULT_CONFIG,
			logReductionProvider: "test-provider",
			logReductionModel: "test-reducer-model",
		});
	});

	it.each([
		["logReductionProvider", ""],
		["logReductionProvider", 12],
		["logReductionModel", ""],
		["logReductionModel", 12],
	] as const)("rejects an invalid EPR reducer string: %s=%j", (key, value) => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "context.json");
		writeFileSync(path, JSON.stringify({ version: 1, [key]: value }));
		expect(() => loadContextConfig(cwd, agentDir, true)).toThrow(`Context config ${key} must be a non-empty string`);
	});

	it.each([null, "12.5", -1])("rejects an invalid cache write/read ratio: %j", (cacheWriteReadRatio) => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "context.json");
		writeFileSync(path, JSON.stringify({ version: 1, cacheWriteReadRatio }));
		expect(() => loadContextConfig(cwd, agentDir, true)).toThrow(
			"Context config cacheWriteReadRatio must be a finite non-negative number",
		);
	});

	it("wraps malformed JSON errors with the config path", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		const path = join(cwd, CONFIG_DIR_NAME, "context.json");
		writeFileSync(path, "{");
		expect(() => loadContextConfig(cwd, agentDir, true)).toThrow(`Unable to read Context config ${path}`);
	});
});
