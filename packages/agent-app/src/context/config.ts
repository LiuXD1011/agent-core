/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";
import { CONFIG_DIR_NAME, getAgentDir } from "../config.ts";
import { DEFAULT_REDUCER_MODEL, DEFAULT_REDUCER_PROVIDER } from "./reduction/config.ts";

export const DEFAULT_CACHE_WRITE_READ_RATIO = 12.5;

export interface ContextConfig {
	readonly version: 1;
	readonly mutationCommands: boolean;
	readonly resultReferences: boolean;
	readonly logReduction: boolean;
	readonly logReductionModel: string;
	readonly logReductionProvider: string;
	readonly boundaryCompaction: boolean;
	readonly cacheWriteReadRatio: number;
}

export const DEFAULT_CONFIG: ContextConfig = Object.freeze({
	version: 1,
	mutationCommands: true,
	resultReferences: true,
	logReduction: true,
	logReductionModel: DEFAULT_REDUCER_MODEL,
	logReductionProvider: DEFAULT_REDUCER_PROVIDER,
	boundaryCompaction: true,
	cacheWriteReadRatio: DEFAULT_CACHE_WRITE_READ_RATIO,
});

const FEATURE_KEYS = ["mutationCommands", "resultReferences", "logReduction", "boundaryCompaction"] as const;
const STRING_KEYS = ["logReductionModel", "logReductionProvider"] as const;
const CONFIG_KEYS = new Set<string>(["version", ...FEATURE_KEYS, ...STRING_KEYS, "cacheWriteReadRatio"]);

export function findConfigPath(
	cwd = process.cwd(),
	agentDir = getAgentDir(),
	allowProjectConfig = false,
): string | undefined {
	if (allowProjectConfig) {
		const projectPath = join(cwd, CONFIG_DIR_NAME, "context.json");
		if (existsSync(projectPath)) return projectPath;
	}

	const globalPath = join(agentDir, "context.json");
	return existsSync(globalPath) ? globalPath : undefined;
}

export function loadContextConfig(
	cwd = process.cwd(),
	agentDir = getAgentDir(),
	allowProjectConfig = false,
): ContextConfig {
	const path = findConfigPath(cwd, agentDir, allowProjectConfig);
	if (!path) return DEFAULT_CONFIG;

	let parsed: unknown;
	try {
		parsed = JSON.parse(readFileSync(path, "utf8"));
	} catch (error) {
		const reason = error instanceof Error ? error.message : String(error);
		throw new Error(`Unable to read Context config ${path}: ${reason}`);
	}

	if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
		throw new Error(`Context config must be a JSON object: ${path}`);
	}

	const record = parsed as Record<string, unknown>;
	for (const key of Object.keys(record)) {
		if (!CONFIG_KEYS.has(key)) throw new Error(`Unknown Context config key: ${key}`);
	}
	if (record.version !== 1) throw new Error(`Context config version must be 1: ${path}`);

	for (const key of FEATURE_KEYS) {
		if (record[key] !== undefined && typeof record[key] !== "boolean") {
			throw new Error(`Context config ${key} must be boolean: ${path}`);
		}
	}
	const cacheWriteReadRatio = Object.hasOwn(record, "cacheWriteReadRatio")
		? record.cacheWriteReadRatio
		: DEFAULT_CACHE_WRITE_READ_RATIO;
	if (typeof cacheWriteReadRatio !== "number" || !Number.isFinite(cacheWriteReadRatio) || cacheWriteReadRatio < 0) {
		throw new Error(`Context config cacheWriteReadRatio must be a finite non-negative number: ${path}`);
	}
	const logReductionModel = stringConfigValue(record, "logReductionModel", DEFAULT_REDUCER_MODEL, path);
	const logReductionProvider = stringConfigValue(record, "logReductionProvider", DEFAULT_REDUCER_PROVIDER, path);

	if ((logReductionModel === "$current") !== (logReductionProvider === "$current")) {
		throw new Error("Context reducer provider and model must both use $current or both name an explicit route");
	}
	return Object.freeze({
		...DEFAULT_CONFIG,
		...record,
		cacheWriteReadRatio,
		logReductionModel,
		logReductionProvider,
	}) as ContextConfig;
}

function stringConfigValue(
	record: Record<string, unknown>,
	key: (typeof STRING_KEYS)[number],
	defaultValue: string,
	path: string,
): string {
	const value = Object.hasOwn(record, key) ? record[key] : defaultValue;
	if (typeof value !== "string" || value.trim().length === 0) {
		throw new Error(`Context config ${key} must be a non-empty string: ${path}`);
	}
	return value;
}
