/*
 * SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
 */

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AssistantMessage, Context, Model } from "@liuxuedeng/agent-core-ai";
import { describe, expect, it } from "vitest";
import { createReducerRunner, REDUCER_RECEIPT_SCHEMA } from "../../src/context/reduction/index.ts";
import { runtimeRoot } from "../../src/session/artifact-paths.ts";
import { FakePi, fakeContext } from "./helpers.ts";
import type { ExtensionContext, ToolResultEvent } from "./host.ts";

const CUSTOM_REDUCER = {
	id: "configured-reducer-model",
	name: "configured reducer model",
	api: "openai-responses",
	provider: "configured-provider",
	baseUrl: "https://example.invalid/v1",
	reasoning: true,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 32_768,
	maxTokens: 4_096,
} satisfies Model<"openai-responses">;

function contextInput(context: Context): string {
	const message = context.messages[0];
	if (message?.role !== "user") throw new Error("reducer request omitted its user message");
	if (typeof message.content === "string") return message.content;
	return message.content.flatMap((item) => (item.type === "text" ? [item.text] : [])).join("\n");
}

function sourceHash(input: string): string {
	const match = input.match(/source_sha256=([a-f0-9]{64})/u);
	if (!match?.[1]) throw new Error("request omitted source hash");
	return match[1];
}

function bashEvent(body: string): ToolResultEvent {
	return {
		type: "tool_result",
		toolName: "bash",
		toolCallId: "call-configured-reducer",
		input: { command: "pytest -q" },
		content: [{ type: "text", text: body }],
		details: undefined,
		isError: true,
	} as ToolResultEvent;
}

describe("Context entrypoint", () => {
	it("passes the configured reducer provider/model route into EPR", async () => {
		const root = mkdtempSync(join(tmpdir(), "context-configured-epr-"));
		try {
			const runner = createReducerRunner({
				reducerProvider: CUSTOM_REDUCER.provider,
				reducerModel: CUSTOM_REDUCER.id,
			});
			const pi = new FakePi();
			const body = `ERROR configured reducer failure\n${"diagnostic line\n".repeat(360)}`;
			let calledModel: Model<string> | undefined;
			const context = fakeContext(root, {
				modelRegistry: {
					find: (provider: string, modelId: string) =>
						provider === CUSTOM_REDUCER.provider && modelId === CUSTOM_REDUCER.id ? CUSTOM_REDUCER : undefined,
					complete: async (model: Model<string>, request: Context): Promise<AssistantMessage> => {
						calledModel = model;
						const input = contextInput(request);
						return {
							role: "assistant",
							content: [
								{
									type: "text",
									text: JSON.stringify({
										schema: REDUCER_RECEIPT_SCHEMA,
										source_sha256: sourceHash(input),
										status: "failure",
										uncertain: false,
										evidence: [{ kind: "failure", quote: "ERROR configured reducer failure" }],
									}),
								},
							],
							api: model.api,
							provider: model.provider,
							model: model.id,
							usage: {
								input: 10,
								output: 10,
								cacheRead: 0,
								cacheWrite: 0,
								totalTokens: 20,
								cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
							},
							stopReason: "stop",
							timestamp: Date.now(),
						};
					},
				} as unknown as ExtensionContext["modelRegistry"],
			});

			const result = (await runner(bashEvent(body), {
				getRuntimeRoot: () => runtimeRoot(context),
				appendSessionEntry: (customType, data) =>
					(
						pi.sessionManager as unknown as { appendCustomEntry: (t: string, d: unknown) => void }
					).appendCustomEntry(customType, data),
				modelRegistry: context.modelRegistry,
				getModel: () => context.model,
				getSignal: () => context.signal,
				showSavings: () => {},
			})) as { content: { type: string; text: string }[] };

			expect(calledModel).toBe(CUSTOM_REDUCER);
			expect(result.content[0]?.text ?? "").toContain(`reducer_model=${CUSTOM_REDUCER.id}`);
			expect(result.content[0]?.text ?? "").toContain(`reducer_provider=${CUSTOM_REDUCER.provider}`);
		} finally {
			rmSync(root, { recursive: true, force: true });
		}
	});
});
