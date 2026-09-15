/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { type EfficiencyConfig, loadEfficiencyConfig } from "./config.ts";
import { type ActionFusionOptions, createActionFusionExtension } from "./extensions/action-fusion/index.ts";
import { registerEvidencePreservingReducer } from "./extensions/evidence-preserving-reducer/index.ts";
import { registerObservationPack } from "./extensions/observation-pack/index.ts";
import { createOnlineContextCompactExtension } from "./extensions/online-context-compact/index.ts";
import { type ExtensionAPI, type ExtensionContext, type ExtensionFactory, getAgentDir } from "./host.ts";

export interface EfficiencyHostOptions {
	actionFusion?: ActionFusionOptions;
	keepRecentTokens?: number;
	deferActionFusion?: boolean;
}

export function registerConfiguredFeatures(
	pi: ExtensionAPI,
	config: EfficiencyConfig,
	options: EfficiencyHostOptions = {},
): void {
	if (config.actionFusion) {
		if (options.deferActionFusion) {
			let initialized = false;
			pi.on("session_start", () => {
				if (initialized) return;
				initialized = true;
				if (options.actionFusion?.canFuse && !options.actionFusion.canFuse()) return;
				createActionFusionExtension({ ...options.actionFusion, tools: pi.getActiveTools() })(pi);
			});
		} else createActionFusionExtension(options.actionFusion)(pi);
	}
	if (config.observationPack) registerObservationPack(pi);
	if (config.evidencePreservingReducer) {
		registerEvidencePreservingReducer(pi, {
			reducerModel: config.evidencePreservingReducerModel,
			reducerProvider: config.evidencePreservingReducerProvider,
		});
	}
	if (config.onlineContextCompact)
		createOnlineContextCompactExtension({
			cacheWriteReadRatio: config.cacheWriteReadRatio,
			keepRecentTokens: options.keepRecentTokens,
		})(pi);
}

export type EfficiencyConfigLoader = (ctx: ExtensionContext) => EfficiencyConfig;

export function createEfficiencyRuntime(
	loadConfig: EfficiencyConfigLoader = (ctx) => loadEfficiencyConfig(ctx.cwd, getAgentDir(), ctx.isProjectTrusted()),
): ExtensionFactory {
	return (pi) => {
		let initialized = false;
		pi.on("session_start", (_event, ctx) => {
			if (initialized) return;
			initialized = true;
			registerConfiguredFeatures(pi, loadConfig(ctx));
		});
	};
}

export default function efficiencyRuntime(pi: ExtensionAPI): void {
	createEfficiencyRuntime()(pi);
}
