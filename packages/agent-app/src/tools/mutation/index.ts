/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */
/** File mutation followed by an optional validation command, in one tool call. */

import { Type } from "typebox";
import type { ToolDefinition } from "../../extensions/types.ts";
import type { BashToolOptions } from "../bash.ts";
import { createEditToolDefinition, type EditToolDetails, type EditToolOptions } from "../edit.ts";
import { createWriteToolDefinition, type WriteToolOptions } from "../write.ts";
import { resolveToolPath } from "./file-queue.ts";
import { createThenRunSchema, executeMutationThenRun, type ThenRunInput } from "./then-run.ts";

const EDIT_THEN_RUN_DESCRIPTION =
	"Command to run next on this file after the edit succeeds — e.g. run, build, start/restart, install, or check it; optional timeout in seconds. Skipped if the edit fails; a non-zero exit is reported but keeps the edit.";
const WRITE_THEN_RUN_DESCRIPTION =
	"Command to run next on this file after the write succeeds — e.g. run, build, start/restart, install, or check it; optional timeout in seconds. Skipped if the write fails; a non-zero exit is reported but keeps the write.";

export interface ActionFusionOptions {
	/** Host guard, rechecked before mutation and before the follow-up command. */
	readonly canFuse?: () => boolean;
	readonly tools?: readonly string[];
	/** Optional programmatic bash overrides, primarily for tests and embedded runtimes. */
	readonly bashOptions?: BashToolOptions;
	/** Overrides for the underlying built-in `edit` tool. */
	readonly editOptions?: EditToolOptions;
	/** Overrides for the underlying built-in `write` tool. */
	readonly writeOptions?: WriteToolOptions;
}

/**
 * Built-in tool definitions capture their cwd in closures, so keep one per
 * working directory instead of rebuilding them on every call and every redraw.
 */
function memoizeByCwd<T>(create: (cwd: string) => T): (cwd: string) => T {
	const cache = new Map<string, T>();
	return (cwd) => {
		const cached = cache.get(cwd);
		if (cached) return cached;
		const created = create(cwd);
		cache.set(cwd, created);
		return created;
	};
}

/**
 * Native fused mutation definitions: the built-in edit/write tools plus an
 * optional `then_run` command executed after a successful mutation.
 */
export function createFusedMutationToolDefinitions(options: ActionFusionOptions = {}) {
	const baseEdit = memoizeByCwd((cwd: string) => createEditToolDefinition(cwd, options.editOptions));
	const baseWrite = memoizeByCwd((cwd: string) => createWriteToolDefinition(cwd, options.writeOptions));

	const editTemplate = baseEdit(process.cwd());
	const writeTemplate = baseWrite(process.cwd());

	const editParameters = Type.Object({
		...editTemplate.parameters.properties,
		then_run: createThenRunSchema(EDIT_THEN_RUN_DESCRIPTION),
	});
	const writeParameters = Type.Object({
		...writeTemplate.parameters.properties,
		then_run: createThenRunSchema(WRITE_THEN_RUN_DESCRIPTION),
	});

	const edit: ToolDefinition<typeof editParameters, EditToolDetails | undefined> = {
		...editTemplate,
		promptGuidelines: [
			...(editTemplate.promptGuidelines ?? []),
			"Use then_run for a known follow-up validation command after a successful edit; inspect its result before continuing.",
		],
		parameters: editParameters,
		async execute(toolCallId, input, signal, onUpdate, ctx) {
			const { then_run, ...editInput } = input as typeof input & { then_run?: ThenRunInput };
			if (then_run && options.canFuse && !options.canFuse()) {
				throw new Error("[then_run:skipped] Use separate mutation and bash calls under the current tool policy.");
			}
			const result = await executeMutationThenRun({
				canRun: options.canFuse,
				toolCallId,
				absolutePath: resolveToolPath(ctx.cwd, input.path),
				thenRun: then_run,
				bashOptions: options.bashOptions,
				signal,
				ctx,
				mutate: () => baseEdit(ctx.cwd).execute(toolCallId, editInput, signal, onUpdate, ctx),
			});
			return result;
		},
	};

	const write: ToolDefinition<typeof writeParameters, undefined> = {
		...writeTemplate,
		promptGuidelines: [
			...(writeTemplate.promptGuidelines ?? []),
			"Use then_run for a known follow-up validation command after a successful write; inspect its result before continuing.",
		],
		parameters: writeParameters,
		async execute(toolCallId, input, signal, onUpdate, ctx) {
			const { then_run, ...writeInput } = input as typeof input & { then_run?: ThenRunInput };
			if (then_run && options.canFuse && !options.canFuse()) {
				throw new Error("[then_run:skipped] Use separate mutation and bash calls under the current tool policy.");
			}
			const result = await executeMutationThenRun({
				canRun: options.canFuse,
				toolCallId,
				absolutePath: resolveToolPath(ctx.cwd, input.path),
				thenRun: then_run,
				bashOptions: options.bashOptions,
				signal,
				ctx,
				mutate: () => baseWrite(ctx.cwd).execute(toolCallId, writeInput, signal, onUpdate, ctx),
			});
			return result;
		},
	};

	return { edit, write };
}

export type { ThenRunInput } from "./then-run.ts";
export {
	assertUnchangedBeforeCommand,
	executeMutationThenRun,
	THEN_RUN_FAILED,
	THEN_RUN_SKIPPED,
	THEN_RUN_SUCCEEDED,
} from "./then-run.ts";
