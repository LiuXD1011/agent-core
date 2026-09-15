/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import { DEFAULT_CACHE_WRITE_READ_RATIO } from "../../config.ts";
import type { ExtensionAPI } from "../../host.ts";
import { createOnlineContextCompactExtension } from "./extension.ts";

export {
	type CompactionDecision,
	type CompactionEconomics,
	type CompactionReason,
	DEFAULT_COMPACTION_ECONOMICS,
	decideCompaction,
	estimateRemainingRequests,
} from "./economics.ts";
export {
	BOUNDARY_COMPACTION_INSTRUCTIONS,
	createOnlineContextCompactExtension,
	DEFAULT_KEEP_RECENT_TOKENS,
	DEFAULT_NATIVE_SUMMARY_TOKEN_ESTIMATE,
	type OnlineContextCompactOptions,
	POST_COMPACTION_PLAN_REMINDER,
	resolveKeepRecentTokens,
} from "./extension.ts";
export {
	analyzePlanTransition,
	formatPlanSnapshot,
	type PlanStatus,
	type PlanStep,
	parsePlanSteps,
} from "./plan.ts";
export {
	initialOnlineState,
	ONLINE_STATE_ENTRY,
	type OnlineState,
	type ProgressSummary,
	restoreOnlineState,
} from "./state.ts";
export type { PlanProgress, PlanUpdateInput } from "./tools.ts";

export function registerOnlineContextCompact(
	pi: ExtensionAPI,
	cacheWriteReadRatio = DEFAULT_CACHE_WRITE_READ_RATIO,
): void {
	createOnlineContextCompactExtension({ cacheWriteReadRatio })(pi);
}

export default registerOnlineContextCompact;
