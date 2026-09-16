/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */
import type { AgentToolResult } from "@liuxuedeng/agent-core-agent";
import { Type } from "typebox";
import { PLAN_STATUSES, type PlanStep } from "../context/plan/plan.ts";
import type { ExtensionContext, ToolDefinition } from "../extensions/types.ts";

export type PlanProgress = {
	readonly files_changed: readonly string[];
	readonly verification: readonly string[];
	readonly decisions: readonly string[];
};

export type PlanUpdateInput = {
	readonly toolCallId: string;
	readonly steps: readonly PlanStep[];
	readonly progress: PlanProgress | undefined;
	readonly signal: AbortSignal | undefined;
	/** Extension execution context when the tool runs through an extension; native runs omit it. */
	readonly context?: ExtensionContext;
};

export type OnlineToolHandlers = {
	readonly updatePlan: (input: PlanUpdateInput) => Promise<AgentToolResult<Readonly<Record<string, unknown>>>>;
};

const progressSchema = Type.Object(
	{
		files_changed: Type.Array(Type.String({ maxLength: 1000 }), { maxItems: 128 }),
		verification: Type.Array(Type.String({ maxLength: 1000 }), { maxItems: 64 }),
		decisions: Type.Array(Type.String({ maxLength: 1000 }), { maxItems: 64 }),
	},
	{ additionalProperties: false },
);

const planStepSchema = Type.Object(
	{
		id: Type.String({ minLength: 1, maxLength: 16_384 }),
		goal: Type.String({ minLength: 1, maxLength: 16_384 }),
		status: Type.Union(PLAN_STATUSES.map((status) => Type.Literal(status))),
	},
	{ additionalProperties: false },
);

export const updatePlanParameters = Type.Object(
	{
		steps: Type.Array(planStepSchema, { minItems: 1, maxItems: 128 }),
		progress: Type.Optional(progressSchema),
	},
	{ additionalProperties: false },
);

/**
 * Native update_plan tool definition. Renderers ride along for display layers;
 * execution goes through the handlers without any extension context.
 */
export function createUpdatePlanToolDefinition(
	handlers: OnlineToolHandlers,
	decorate?: Pick<ToolDefinition<typeof updatePlanParameters>, "renderCall" | "renderResult">,
): ToolDefinition<typeof updatePlanParameters> {
	return {
		name: "update_plan",
		label: "Update plan",
		description:
			"Replace the complete working plan. A newly completed step becomes a safe point where the agent may compact context if doing so is economical.",
		promptSnippet: "Keep the working plan current",
		promptGuidelines: [
			"For multi-step tasks, create a plan before starting and update it as steps finish. Skip planning for trivial single-step requests.",
			"Send the complete plan on every update_plan call.",
			"Keep at most one step in_progress and mark finished steps completed.",
			"When completing a step, include concise progress evidence when available.",
		],
		parameters: updatePlanParameters,
		executionMode: "sequential",
		execute: async (toolCallId, params, signal, _onUpdate, context) =>
			await handlers.updatePlan({
				toolCallId,
				steps: params.steps,
				progress: params.progress,
				signal,
				context,
			}),
		...(decorate ?? {}),
	};
}
