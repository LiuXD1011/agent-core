/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { afterEach, describe, expect, it } from "vitest";
import { DEFAULT_CONFIG, findConfigPath, loadEfficiencyConfig } from "../../src/core/efficiency/config.ts";
import {
	DEFAULT_REDUCER_MODEL,
	DEFAULT_REDUCER_PROVIDER,
} from "../../src/core/efficiency/extensions/evidence-preserving-reducer/config.ts";
import { CONFIG_DIR_NAME } from "./host.ts";

const roots: string[] = [];

function fixture(): { agentDir: string; cwd: string } {
	const root = mkdtempSync(join(tmpdir(), "efficiency-config-"));
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

describe("Efficiency config", () => {
	it("returns disabled defaults when neither config exists", () => {
		const { agentDir, cwd } = fixture();
		expect(findConfigPath(cwd, agentDir, true)).toBeUndefined();
		expect(loadEfficiencyConfig(cwd, agentDir, true)).toEqual(DEFAULT_CONFIG);
		expect(DEFAULT_CONFIG.cacheWriteReadRatio).toBe(12.5);
		expect(DEFAULT_CONFIG.evidencePreservingReducerProvider).toBe(DEFAULT_REDUCER_PROVIDER);
		expect(DEFAULT_CONFIG.evidencePreservingReducerModel).toBe(DEFAULT_REDUCER_MODEL);
	});

	it("loads the global config as a fallback", () => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "efficiency.json");
		writeFileSync(path, JSON.stringify({ version: 1, observationPack: true, onlineContextCompact: true }));
		expect(findConfigPath(cwd, agentDir, true)).toBe(path);
		expect(loadEfficiencyConfig(cwd, agentDir, true)).toEqual({
			...DEFAULT_CONFIG,
			observationPack: true,
			onlineContextCompact: true,
		});
	});

	it("uses the project config instead of merging the global config", () => {
		const { agentDir, cwd } = fixture();
		writeFileSync(join(agentDir, "efficiency.json"), JSON.stringify({ version: 1, observationPack: true }));
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		const projectPath = join(cwd, CONFIG_DIR_NAME, "efficiency.json");
		writeFileSync(projectPath, JSON.stringify({ version: 1, actionFusion: true }));

		expect(findConfigPath(cwd, agentDir, true)).toBe(projectPath);
		expect(loadEfficiencyConfig(cwd, agentDir, true)).toEqual({ ...DEFAULT_CONFIG, actionFusion: true });
	});

	it("ignores the project config when Pi has not trusted the project", () => {
		const { agentDir, cwd } = fixture();
		writeFileSync(join(agentDir, "efficiency.json"), JSON.stringify({ version: 1, observationPack: true }));
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(join(cwd, CONFIG_DIR_NAME, "efficiency.json"), JSON.stringify({ version: 1, actionFusion: true }));

		expect(loadEfficiencyConfig(cwd, agentDir, false)).toEqual({ ...DEFAULT_CONFIG, observationPack: true });
	});

	it("rejects unknown keys", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(join(cwd, CONFIG_DIR_NAME, "efficiency.json"), JSON.stringify({ version: 1, actionFussion: true }));
		expect(() => loadEfficiencyConfig(cwd, agentDir, true)).toThrow("Unknown Efficiency config key: actionFussion");
	});

	it("rejects non-boolean feature values", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(join(cwd, CONFIG_DIR_NAME, "efficiency.json"), JSON.stringify({ version: 1, actionFusion: "yes" }));
		expect(() => loadEfficiencyConfig(cwd, agentDir, true)).toThrow("Efficiency config actionFusion must be boolean");
	});

	it("rejects a non-boolean Online Context Compact value", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		writeFileSync(
			join(cwd, CONFIG_DIR_NAME, "efficiency.json"),
			JSON.stringify({ version: 1, onlineContextCompact: "yes" }),
		);
		expect(() => loadEfficiencyConfig(cwd, agentDir, true)).toThrow(
			"Efficiency config onlineContextCompact must be boolean",
		);
	});

	it("loads an explicit cache write/read ratio, including zero", () => {
		for (const cacheWriteReadRatio of [0, 3.25]) {
			const { agentDir, cwd } = fixture();
			const path = join(agentDir, "efficiency.json");
			writeFileSync(path, JSON.stringify({ version: 1, cacheWriteReadRatio }));
			expect(loadEfficiencyConfig(cwd, agentDir, true).cacheWriteReadRatio).toBe(cacheWriteReadRatio);
		}
	});

	it("loads an explicit Evidence-Preserving Reducer provider/model route", () => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "efficiency.json");
		writeFileSync(
			path,
			JSON.stringify({
				version: 1,
				evidencePreservingReducerProvider: "test-provider",
				evidencePreservingReducerModel: "test-reducer-model",
			}),
		);
		expect(loadEfficiencyConfig(cwd, agentDir, true)).toEqual({
			...DEFAULT_CONFIG,
			evidencePreservingReducerProvider: "test-provider",
			evidencePreservingReducerModel: "test-reducer-model",
		});
	});

	it.each([
		["evidencePreservingReducerProvider", ""],
		["evidencePreservingReducerProvider", 12],
		["evidencePreservingReducerModel", ""],
		["evidencePreservingReducerModel", 12],
	] as const)("rejects an invalid EPR reducer string: %s=%j", (key, value) => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "efficiency.json");
		writeFileSync(path, JSON.stringify({ version: 1, [key]: value }));
		expect(() => loadEfficiencyConfig(cwd, agentDir, true)).toThrow(
			`Efficiency config ${key} must be a non-empty string`,
		);
	});

	it.each([null, "12.5", -1])("rejects an invalid cache write/read ratio: %j", (cacheWriteReadRatio) => {
		const { agentDir, cwd } = fixture();
		const path = join(agentDir, "efficiency.json");
		writeFileSync(path, JSON.stringify({ version: 1, cacheWriteReadRatio }));
		expect(() => loadEfficiencyConfig(cwd, agentDir, true)).toThrow(
			"Efficiency config cacheWriteReadRatio must be a finite non-negative number",
		);
	});

	it("wraps malformed JSON errors with the config path", () => {
		const { agentDir, cwd } = fixture();
		mkdirSync(join(cwd, CONFIG_DIR_NAME));
		const path = join(cwd, CONFIG_DIR_NAME, "efficiency.json");
		writeFileSync(path, "{");
		expect(() => loadEfficiencyConfig(cwd, agentDir, true)).toThrow(`Unable to read Efficiency config ${path}`);
	});
});
