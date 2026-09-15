/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { DEFAULT_REDUCER_MODEL, DEFAULT_REDUCER_PROVIDER } from "./extensions/evidence-preserving-reducer/config.ts";
import { CONFIG_DIR_NAME, getAgentDir } from "./host.ts";

export const DEFAULT_CACHE_WRITE_READ_RATIO = 12.5;

export interface EfficiencyConfig {
	readonly version: 1;
	readonly actionFusion: boolean;
	readonly observationPack: boolean;
	readonly evidencePreservingReducer: boolean;
	readonly evidencePreservingReducerModel: string;
	readonly evidencePreservingReducerProvider: string;
	readonly onlineContextCompact: boolean;
	readonly cacheWriteReadRatio: number;
}

export const DEFAULT_CONFIG: EfficiencyConfig = Object.freeze({
	version: 1,
	actionFusion: false,
	observationPack: false,
	evidencePreservingReducer: false,
	evidencePreservingReducerModel: DEFAULT_REDUCER_MODEL,
	evidencePreservingReducerProvider: DEFAULT_REDUCER_PROVIDER,
	onlineContextCompact: false,
	cacheWriteReadRatio: DEFAULT_CACHE_WRITE_READ_RATIO,
});

const FEATURE_KEYS = ["actionFusion", "observationPack", "evidencePreservingReducer", "onlineContextCompact"] as const;
const STRING_KEYS = ["evidencePreservingReducerModel", "evidencePreservingReducerProvider"] as const;
const CONFIG_KEYS = new Set<string>(["version", ...FEATURE_KEYS, ...STRING_KEYS, "cacheWriteReadRatio"]);

export function findConfigPath(
	cwd = process.cwd(),
	agentDir = getAgentDir(),
	allowProjectConfig = false,
): string | undefined {
	if (allowProjectConfig) {
		const projectPath = join(cwd, CONFIG_DIR_NAME, "efficiency.json");
		if (existsSync(projectPath)) return projectPath;
	}

	const globalPath = join(agentDir, "efficiency.json");
	return existsSync(globalPath) ? globalPath : undefined;
}

export function loadEfficiencyConfig(
	cwd = process.cwd(),
	agentDir = getAgentDir(),
	allowProjectConfig = false,
): EfficiencyConfig {
	const path = findConfigPath(cwd, agentDir, allowProjectConfig);
	if (!path) return DEFAULT_CONFIG;

	let parsed: unknown;
	try {
		parsed = JSON.parse(readFileSync(path, "utf8"));
	} catch (error) {
		const reason = error instanceof Error ? error.message : String(error);
		throw new Error(`Unable to read Efficiency config ${path}: ${reason}`);
	}

	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		throw new Error(`Efficiency config must be a JSON object: ${path}`);
	}

	const record = parsed as Record<string, unknown>;
	for (const key of Object.keys(record)) {
		if (!CONFIG_KEYS.has(key)) throw new Error(`Unknown Efficiency config key: ${key}`);
	}
	if (record.version !== 1) throw new Error(`Efficiency config version must be 1: ${path}`);

	for (const key of FEATURE_KEYS) {
		if (record[key] !== undefined && typeof record[key] !== "boolean") {
			throw new Error(`Efficiency config ${key} must be boolean: ${path}`);
		}
	}
	const cacheWriteReadRatio = Object.hasOwn(record, "cacheWriteReadRatio")
		? record.cacheWriteReadRatio
		: DEFAULT_CACHE_WRITE_READ_RATIO;
	if (typeof cacheWriteReadRatio !== "number" || !Number.isFinite(cacheWriteReadRatio) || cacheWriteReadRatio < 0) {
		throw new Error(`Efficiency config cacheWriteReadRatio must be a finite non-negative number: ${path}`);
	}
	const evidencePreservingReducerModel = stringConfigValue(
		record,
		"evidencePreservingReducerModel",
		DEFAULT_REDUCER_MODEL,
		path,
	);
	const evidencePreservingReducerProvider = stringConfigValue(
		record,
		"evidencePreservingReducerProvider",
		DEFAULT_REDUCER_PROVIDER,
		path,
	);

	if ((evidencePreservingReducerModel === "$current") !== (evidencePreservingReducerProvider === "$current")) {
		throw new Error("Efficiency reducer provider and model must both use $current or both name an explicit route");
	}
	return Object.freeze({
		...DEFAULT_CONFIG,
		...record,
		cacheWriteReadRatio,
		evidencePreservingReducerModel,
		evidencePreservingReducerProvider,
	}) as EfficiencyConfig;
}

function stringConfigValue(
	record: Record<string, unknown>,
	key: (typeof STRING_KEYS)[number],
	defaultValue: string,
	path: string,
): string {
	const value = Object.hasOwn(record, key) ? record[key] : defaultValue;
	if (typeof value !== "string" || value.trim().length === 0) {
		throw new Error(`Efficiency config ${key} must be a non-empty string: ${path}`);
	}
	return value;
}
