/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 * SPDX-License-Identifier: MIT
 */

import type { AssistantMessage } from "@liuxuedeng/agent-core-ai";
import type { ExtensionContext } from "../../extensions/types.ts";
import type { ArchiveObject } from "./archive.ts";
import type { ReducerConfig } from "./config.ts";
import { reducerInput, reducerInstructions } from "./receipt.ts";

export interface NormalizedUsage {
	readonly input: number;
	readonly output: number;
	readonly cacheRead: number;
	readonly cacheWrite: number;
	readonly totalTokens: number;
}

export interface ProviderResult {
	readonly errorMessage: string | undefined;
	readonly model: string;
	readonly ok: boolean;
	readonly outputText: string;
	readonly provider: string;
	readonly stopReason: AssistantMessage["stopReason"];
	readonly usage: NormalizedUsage;
}

export class ReducerModelUnavailableError extends Error {
	override readonly name = "ReducerModelUnavailableError";
}

function responseOutputText(response: AssistantMessage): string {
	return response.content.flatMap((item) => (item.type === "text" ? [item.text] : [])).join("");
}

function normalizedUsage(response: AssistantMessage): NormalizedUsage {
	return {
		input: response.usage.input,
		output: response.usage.output,
		cacheRead: response.usage.cacheRead,
		cacheWrite: response.usage.cacheWrite,
		totalTokens: response.usage.totalTokens,
	};
}

function operationSignal(
	parent: AbortSignal | undefined,
	timeoutMs: number,
): {
	readonly cleanup: () => void;
	readonly signal: AbortSignal;
} {
	const controller = new AbortController();
	const relayAbort = () => controller.abort(parent?.reason);
	if (parent?.aborted) relayAbort();
	else parent?.addEventListener("abort", relayAbort, { once: true });
	const timer = setTimeout(
		() => controller.abort(new DOMException("Reducer model call timed out", "AbortError")),
		timeoutMs,
	);
	return {
		signal: controller.signal,
		cleanup: () => {
			clearTimeout(timer);
			parent?.removeEventListener("abort", relayAbort);
		},
	};
}

/** Use the configured reducer model and Pi-managed authentication for the reducer call. */
export async function callReducer(
	config: ReducerConfig,
	command: string,
	isError: boolean,
	archive: ArchiveObject,
	body: string,
	context: ExtensionContext,
): Promise<ProviderResult> {
	const registry = context.modelRegistry;
	const model =
		config.reducerProvider === "$current" && config.reducerModel === "$current"
			? context.model
			: registry.find(config.reducerProvider, config.reducerModel);
	if (!model)
		throw new ReducerModelUnavailableError(
			`Reducer model is unavailable: ${config.reducerProvider}/${config.reducerModel}`,
		);
	context.signal?.throwIfAborted();
	const operation = operationSignal(context.signal, config.timeoutMs);
	try {
		const requestContext = {
			systemPrompt: reducerInstructions(),
			messages: [
				{
					role: "user" as const,
					content: [{ type: "text" as const, text: reducerInput(command, isError, archive, body) }],
					timestamp: Date.now(),
				},
			],
		};
		const requestOptions = {
			cacheRetention: "none" as const,
			maxTokens: Math.min(config.maxOutputTokens, model.maxTokens),
			sessionId: config.runId,
			signal: operation.signal,
			timeoutMs: config.timeoutMs,
		};
		const response = await registry.complete(model, requestContext, requestOptions);
		return {
			errorMessage: response.errorMessage,
			model: response.model,
			ok: response.stopReason === "stop" || response.stopReason === "length",
			outputText: responseOutputText(response),
			provider: response.provider,
			stopReason: response.stopReason,
			usage: normalizedUsage(response),
		};
	} finally {
		operation.cleanup();
	}
}
