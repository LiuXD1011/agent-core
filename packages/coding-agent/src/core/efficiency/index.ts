/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { loadSolPiConfig, type SolPiConfig } from "./config.ts";
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
	config: SolPiConfig,
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

export type SolPiConfigLoader = (ctx: ExtensionContext) => SolPiConfig;

export function createSolPiExtension(
	loadConfig: SolPiConfigLoader = (ctx) => loadSolPiConfig(ctx.cwd, getAgentDir(), ctx.isProjectTrusted()),
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

export default function solPiExtension(pi: ExtensionAPI): void {
	createSolPiExtension()(pi);
}
