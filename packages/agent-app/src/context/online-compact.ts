/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

/**
 * Native boundary-aware online compaction coordinator.
 *
 * Replaces the former `<builtin:context>` online-context-compact extension:
 * the compaction decision runs at the turn boundary inside the same run (via
 * the loop's prepareNextTurn hook), immediately before the next model request.
 * It never aborts the active run, never injects hidden continuation messages,
 * and does not depend on extension loading.
 *
 * Pure decision/economics/bookkeeping logic is reused from economics.ts,
 * plan.ts, and state.ts unchanged, so persisted session state stays compatible.
 */

import type { AgentMessage, AgentToolResult } from "@liuxuedeng/agent-core-agent";
import { type CompactionDecision, DEFAULT_COMPACTION_ECONOMICS, decideCompaction } from "@liuxuedeng/agent-core-agent";
import type { SessionEntry, SessionManager } from "../session/store.ts";
import { sessionEntryToContextMessages } from "../session/store.ts";
import type { PlanUpdateInput } from "../tools/plan.ts";
import {
	analyzePlanTransition,
	BOUNDARY_COMPACTION_INSTRUCTIONS,
	DEFAULT_NATIVE_SUMMARY_TOKEN_ESTIMATE,
	formatPlanSnapshot,
	parsePlanSteps,
} from "./plan/plan.ts";
import {
	appendOnlineState,
	initialOnlineState,
	type OnlineState,
	type ProgressSummary,
	recordBoundary,
	recordCompaction,
	recordCorrection,
	recordProviderRequest,
	restoreOnlineState,
} from "./plan/state.ts";
import { estimateTokens, findCutPoint } from "./summarization/compaction.ts";

export { DEFAULT_KEEP_RECENT_TOKENS, resolveKeepRecentTokens } from "./plan/plan.ts";

/** Everything the coordinator needs from the owning session. */
export interface OnlineCompactionHost {
	readonly sessionManager: SessionManager;
	/** Estimated token usage of the model-visible context, when reported. */
	getContextUsage(): { tokens?: number; contextWindow?: number } | undefined;
	getSystemPrompt(): string;
	/** Fallback context window when usage is not yet reported. */
	getModelContextWindow(): number | null;
	/**
	 * Run a session compaction now. Must not abort or restart the enclosing run;
	 * the loop awaits it at the turn boundary.
	 */
	compact(customInstructions: string | undefined): Promise<{ summary: string } | null>;
	/** Observe removed context tokens (UI notification; failures must not break the run). */
	showSavings?(removedTokens: number): void;
	/** Persist coordinator state as a session custom entry (session_manager.appendCustomEntry). */
	appendEntry(customType: string, data: unknown): void;
}

function tokenEstimate(text: string): number {
	return Math.ceil(Buffer.byteLength(text) / 4);
}

function validPositiveInteger(value: unknown): value is number {
	return typeof value === "number" && Number.isSafeInteger(value) && value > 0;
}

function result(
	text: string,
	details: Readonly<Record<string, unknown>>,
): AgentToolResult<Readonly<Record<string, unknown>>> {
	return { content: [{ type: "text", text }], details };
}

function progressSummary(input: PlanUpdateInput, completedStepId: string): ProgressSummary | undefined {
	const step = input.steps.find((item) => item.id === completedStepId);
	if (!step || !input.progress) return;
	return {
		stepId: step.id,
		goal: step.goal,
		filesChanged: [...input.progress.files_changed],
		verification: [...input.progress.verification],
		decisions: [...input.progress.decisions],
		nextWork: input.steps.filter((item) => item.status !== "completed").map((item) => item.goal),
	};
}

function compactionMessageCount(entries: readonly SessionEntry[], startIndex: number, endIndex: number): number {
	let count = 0;
	for (let index = startIndex; index < endIndex; index++) {
		const entry = entries[index];
		if (entry && entry.type !== "compaction" && sessionEntryToContextMessages(entry).length > 0) count++;
	}
	return count;
}

function nativeCompactionFeasible(entries: readonly SessionEntry[], keepRecentTokens: number): boolean {
	let startIndex = 0;
	for (let index = entries.length - 1; index >= 0; index--) {
		const entry = entries[index];
		if (entry?.type !== "compaction") continue;
		const keptIndex = entries.findIndex((item) => item.id === entry.firstKeptEntryId);
		startIndex = keptIndex >= 0 ? keptIndex : index + 1;
		break;
	}

	const cut = findCutPoint([...entries], startIndex, entries.length, keepRecentTokens);
	const historyEnd = cut.isSplitTurn ? cut.turnStartIndex : cut.firstKeptEntryIndex;
	const historyMessages = historyEnd > startIndex ? compactionMessageCount(entries, startIndex, historyEnd) : 0;
	const prefixMessages =
		cut.isSplitTurn && cut.turnStartIndex >= 0
			? compactionMessageCount(entries, cut.turnStartIndex, cut.firstKeptEntryIndex)
			: 0;
	return historyMessages > 0 || prefixMessages > 0;
}

export class OnlineCompactionCoordinator {
	private state: OnlineState = initialOnlineState();
	private restored = false;
	private pendingBoundary = false;
	private observedMessages: readonly AgentMessage[] = [];
	private readonly keepRecentTokens: number;
	private readonly cacheWriteReadRatio: number | null;

	private readonly host: OnlineCompactionHost;

	constructor(
		host: OnlineCompactionHost,
		options: { keepRecentTokens?: number; cacheWriteReadRatio?: number | null } = {},
	) {
		this.host = host;
		this.keepRecentTokens = options.keepRecentTokens ?? 20_000;
		this.cacheWriteReadRatio = options.cacheWriteReadRatio ?? null;
	}

	restore(): void {
		this.state = restoreOnlineState(this.host.sessionManager.getBranch());
		this.restored = true;
		this.observedMessages = [];
	}

	get stateSnapshot(): OnlineState {
		return this.state;
	}

	private ensureRestored(): void {
		if (!this.restored) this.restore();
	}

	private save(): void {
		appendOnlineState({ appendEntry: this.host.appendEntry }, this.state);
	}

	/** Observe the messages the model is about to see (context projection). */
	observeMessages(messages: readonly AgentMessage[]): void {
		this.observedMessages = [...messages];
	}

	contextTokens(): number {
		const visible = this.observedMessages.reduce((total, message) => total + estimateTokens(message), 0);
		const estimated = visible + tokenEstimate(this.host.getSystemPrompt());
		const reported = this.host.getContextUsage()?.tokens;
		return validPositiveInteger(reported) ? Math.max(reported, estimated) : estimated;
	}

	recordProviderRequest(): void {
		this.ensureRestored();
		this.state = recordProviderRequest(this.state, this.contextTokens());
		if (this.state.plan.length > 0) this.save();
	}

	recordCorrection(): void {
		this.ensureRestored();
		this.pendingBoundary = false;
		this.state = recordCorrection(this.state);
		if (this.state.plan.length > 0) this.save();
	}

	/** Native `update_plan` tool execution. */
	async handlePlanUpdate(
		input: PlanUpdateInput & { toolCallId: string; signal?: AbortSignal },
	): Promise<AgentToolResult<Readonly<Record<string, unknown>>>> {
		this.ensureRestored();
		if (input.signal?.aborted) throw new Error("Plan update was aborted");
		const steps = parsePlanSteps(input.steps);
		if (!steps || steps.length === 0) throw new Error("Plan must contain at least one valid step");

		const transition = analyzePlanTransition(this.state.plan, steps);
		const completedIds = transition.completedSteps.map((step) => step.id);
		if (completedIds.length > 0) {
			this.state = recordBoundary(this.state, steps, progressSummary(input, completedIds[0] ?? ""));
			this.pendingBoundary = true;
			this.save();
		} else if (JSON.stringify(this.state.plan) !== JSON.stringify(steps)) {
			this.state = { ...this.state, plan: [...steps] };
			this.save();
		}

		return result([formatPlanSnapshot(steps), ...transition.advice].join("\n"), {
			boundary: completedIds.length > 0,
			completed_step_ids: completedIds,
			progress_recorded: completedIds.length > 0 && input.progress !== undefined,
			task_status: "active",
			plan: steps,
		});
	}

	/**
	 * Turn-boundary step: decide whether a boundary compaction pays off and, if
	 * so, run it inside the current run. Returns true when the context was
	 * replaced (the caller must rebuild its context snapshot).
	 */
	async maybeCompactBeforeNextTurn(signal: AbortSignal | undefined): Promise<boolean> {
		this.ensureRestored();
		if (signal?.aborted) return false;
		// Compaction decisions run only at a completed-plan boundary, matching the
		// former extension trigger; ordinary turn boundaries never compact.
		if (!this.pendingBoundary) return false;
		this.pendingBoundary = false;

		const writeTokens = this.contextTokens();
		const fixedTokens = tokenEstimate(this.host.getSystemPrompt());
		const archiveTokens = Math.max(0, writeTokens - fixedTokens - this.keepRecentTokens);
		const usage = this.host.getContextUsage();
		const contextWindowTokens = validPositiveInteger(usage?.contextWindow)
			? usage.contextWindow
			: validPositiveInteger(this.host.getModelContextWindow())
				? this.host.getModelContextWindow()
				: null;
		const averageContextTokenIncrement =
			this.state.positiveContextDeltaCount === 0
				? null
				: this.state.positiveContextDeltaTotal / this.state.positiveContextDeltaCount;

		const priced = decideCompaction({
			writeTokens,
			archiveTokens,
			memoTokens: DEFAULT_NATIVE_SUMMARY_TOKEN_ESTIMATE,
			contextTokens: writeTokens,
			completedBoundaryRequestCounts: this.state.completedBoundaryRequestCounts,
			remainingBoundaries: this.state.plan.filter((step) => step.status !== "completed").length,
			averageContextTokenIncrement,
			contextWindowTokens,
			priorCompactionCount: this.state.nativeCompactionCount,
			carriedDebtTokens: this.state.cacheDebtTokens,
			cacheDebtRepaymentTokens: this.state.cacheDebtRepaymentTokens,
			cacheWriteReadRatio: this.cacheWriteReadRatio,
			economics: DEFAULT_COMPACTION_ECONOMICS,
		});
		const decision: CompactionDecision =
			priced.compact && !nativeCompactionFeasible(this.host.sessionManager.getBranch(), this.keepRecentTokens)
				? { ...priced, compact: false, reason: "native_not_compactable" }
				: priced;
		if (!decision.compact) return false;

		const compacted = await this.host.compact(BOUNDARY_COMPACTION_INSTRUCTIONS);
		this.pendingBoundary = false;
		if (!compacted) return false;

		const debtTokens = decision.writeTokens * (decision.incrementalCacheCostRatio ?? 0);
		const repaymentTokens = Math.max(0, decision.archiveTokens - decision.memoTokens);
		this.state = recordCompaction(this.state, { debtTokens, repaymentTokens });
		this.save();

		const removed = Math.max(0, decision.archiveTokens - tokenEstimate(compacted.summary));
		if (removed > 0) {
			this.host.showSavings?.(removed);
		}
		return true;
	}
}
