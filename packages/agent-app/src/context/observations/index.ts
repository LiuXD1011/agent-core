/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */
/**
 * ResultReference - keep large tool results reachable without replaying them.
 *
 * A large tool result is sent in full for its first few provider requests, then
 * replaced with a short, stable placeholder for every later request. The
 * original bytes are archived by observation id outside the provider context,
 * and the agent pulls exact pages back with the registered `obs_recall` tool.
 *
 * The mechanism never edits history in place. It rewrites only at the
 * projection layer, so the stored session stays intact and recall keeps working
 * after native compaction or a session resume.
 *
 * Storage lives under the active session directory.
 */

import { join } from "node:path";
import type { AgentMessage } from "@liuxuedeng/agent-core-agent";
import { Type } from "typebox";
import type { ToolDefinition } from "../../extensions/types.ts";
import { formatSavingsCount } from "../../utils/context-format.ts";
import { createLedger, type Ledger } from "./ledger.ts";
import {
	countLines,
	createObservation,
	ensureStored,
	estimateTokens,
	FULL_SENDS,
	isObservationId,
	isPureTextResult,
	observationPath,
	placeholderFor,
	type RecallChunk,
	readRecallChunk,
} from "./observation.ts";

const RECALL_MAX_BYTES = 16 * 1024;
const RECALL_MAX_LINES = 400;
const RECALL_HEADER_RESERVE_BYTES = 512;
const RECALL_HEADER_LINES = 2;

const RECALL_LIMITS = {
	maxBytes: RECALL_MAX_BYTES - RECALL_HEADER_RESERVE_BYTES,
	maxLines: RECALL_MAX_LINES - RECALL_HEADER_LINES,
};

/** Everything the native assembly provides instead of an extension context. */
export interface ResultReferenceDeps {
	/** Storage root for observations; throws when the session has no persistent dir. */
	getRuntimeRoot(): string;
	/** Whether obs_recall is part of the active tool set (recall must stay reachable). */
	isRecallAvailable(): boolean;
	/** User-visible savings notice; implementations may no-op outside the TUI. */
	showSavings(mechanism: string, saving: string): void;
}

const obsRecallParameters = Type.Object({
	id: Type.String({ description: "Observation id from a placeholder" }),
	offset: Type.Optional(Type.Integer({ minimum: 0, description: "Byte offset, default 0" })),
});

/** The `obs_recall` tool: read a stored observation by id and byte offset. */
export function createObsRecallToolDefinition(deps: ResultReferenceDeps): ToolDefinition<typeof obsRecallParameters> {
	const ledgers = new Map<string, Ledger>();
	const ledgerFor = (): Ledger => {
		const root = deps.getRuntimeRoot();
		let ledger = ledgers.get(root);
		if (!ledger) {
			ledger = createLedger(join(root, "observation-pack", "ledger.jsonl"));
			ledgers.set(root, ledger);
		}
		return ledger;
	};

	return {
		name: "obs_recall",
		label: "Recall Observation",
		description: "Read a stored large tool result by observation id and byte offset.",
		promptSnippet: "Recall a paged excerpt from a previously replaced large tool result",
		parameters: obsRecallParameters,
		async execute(_toolCallId, params, _signal?, _onUpdate?, _ctx?) {
			if (!isObservationId(params.id)) throw new Error(`Unknown observation id: ${params.id}`);
			const offset = params.offset ?? 0;
			let chunk: RecallChunk;
			try {
				chunk = await readRecallChunk(observationPath(deps.getRuntimeRoot(), params.id), offset, RECALL_LIMITS);
			} catch (error) {
				if (error instanceof Error && "code" in error && error.code === "ENOENT") {
					throw new Error(`Unknown observation id: ${params.id}`);
				}
				throw error;
			}
			const header = [
				`[obs_recall id=${params.id} offset=${offset} next_offset=${chunk.nextOffset} eof=${chunk.eof}]`,
				`[chunk_bytes=${chunk.bytes} chunk_lines=${chunk.lines}; use next_offset to continue]`,
			].join("\n");
			const content = `${header}\n${chunk.text}`;
			if (Buffer.byteLength(content, "utf8") > RECALL_MAX_BYTES || countLines(content) > RECALL_MAX_LINES) {
				throw new Error("Recall output exceeded its hard limit");
			}
			await ledgerFor()({
				event: "recall",
				id: params.id,
				offset,
				bytes: chunk.bytes,
				lines: chunk.lines,
				nextOffset: chunk.nextOffset,
				eof: chunk.eof,
			});
			return {
				content: [{ type: "text", text: content }],
				details: {
					id: params.id,
					offset,
					bytes: chunk.bytes,
					lines: chunk.lines,
					nextOffset: chunk.nextOffset,
					eof: chunk.eof,
				},
			};
		},
	};
}

/**
 * The projection layer: replace aged large text results with stable placeholders
 * before each provider request. Fails open — a packing failure never costs the
 * agent its observation.
 */
export function createObservationProjection(deps: ResultReferenceDeps): {
	project(messages: readonly AgentMessage[]): Promise<AgentMessage[]>;
} {
	const sentCounts = new Map<string, number>();
	const ledgers = new Map<string, Ledger>();
	const ledgerFor = (): Ledger => {
		const root = deps.getRuntimeRoot();
		let ledger = ledgers.get(root);
		if (!ledger) {
			ledger = createLedger(join(root, "observation-pack", "ledger.jsonl"));
			ledgers.set(root, ledger);
		}
		return ledger;
	};

	return {
		async project(eventMessages) {
			const projected = [...eventMessages];
			const root = deps.getRuntimeRoot();
			// How many provider requests each message has already been part of,
			// counted by the assistant messages that follow it.
			const priorAssistantCounts = new Array<number>(eventMessages.length);
			let assistantCount = 0;

			for (let index = eventMessages.length - 1; index >= 0; index -= 1) {
				priorAssistantCounts[index] = assistantCount;
				if (eventMessages[index]?.role === "assistant") assistantCount += 1;
			}

			const requestIndex = assistantCount + 1;
			for (let index = 0; index < eventMessages.length; index += 1) {
				const message = eventMessages[index];
				if (!message || !isPureTextResult(message)) continue;

				try {
					const observation = createObservation(message, root);
					if (!observation) continue;
					await ensureStored(observation);

					const sendCountKey = `${root}\0${observation.id}`;
					const previousSends = sentCounts.get(sendCountKey) ?? priorAssistantCounts[index] ?? 0;
					if (previousSends < FULL_SENDS) {
						await ledgerFor()({
							event: "full",
							id: observation.id,
							request: requestIndex,
							tool: observation.toolName,
							originalBytes: observation.bytes,
							originalLines: observation.lines,
							originalTokens: observation.tokens,
							contentHash: observation.contentHash,
						});
						sentCounts.set(sendCountKey, previousSends + 1);
						continue;
					}

					const placeholder = placeholderFor(observation);
					const placeholderTokens = estimateTokens(placeholder);
					const removedTokens = Math.max(0, observation.tokens - placeholderTokens);
					await ledgerFor()({
						event: "placeholder",
						id: observation.id,
						request: requestIndex,
						sendNumber: previousSends + 1,
						tool: observation.toolName,
						originalBytes: observation.bytes,
						originalLines: observation.lines,
						originalTokens: observation.tokens,
						placeholderBytes: Buffer.byteLength(placeholder, "utf8"),
						placeholderTokens,
						removedTokens,
					});
					if (previousSends === FULL_SENDS) {
						deps.showSavings("Result reference", formatSavingsCount(removedTokens, "context tokens avoided"));
					}
					projected[index] = { ...message, content: [{ type: "text", text: placeholder }] };
					sentCounts.set(sendCountKey, previousSends + 1);
				} catch (error) {
					// Fail open: a packing failure must never cost the agent its observation.
					const reason = error instanceof Error ? error.message : String(error);
					console.error(`[observationpack] fail-open for tool result: ${reason}`);
				}
			}

			return projected;
		},
	};
}

export {
	createObservation,
	FULL_SENDS,
	type Observation,
	PLACEHOLDER_EXCERPT_BYTES,
	placeholderFor,
	THRESHOLD_BYTES,
} from "./observation.ts";
