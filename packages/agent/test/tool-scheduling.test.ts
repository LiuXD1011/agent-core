import { fauxAssistantMessage, fauxProvider, fauxToolCall } from "@liuxuedeng/agent-core-ai/providers/faux";
import { Type } from "typebox";
import { describe, expect, it } from "vitest";
import { executeToolCalls } from "../src/tools/execute.ts";
import type { AgentTool } from "../src/types.ts";

const model = fauxProvider({ models: [{ id: "scheduler" }] }).getModel();

describe("bounded tool scheduling", () => {
	it.each(["parallel", "auto"] as const)("bounds %s execution and preserves call order", async (toolExecution) => {
		let running = 0;
		let peak = 0;
		const tool: AgentTool = {
			name: "read",
			label: "Read",
			description: "Read fixture",
			parameters: Type.Object({}),
			executionMode: "parallel",
			execute: async (id) => {
				peak = Math.max(peak, ++running);
				await new Promise((resolve) => setTimeout(resolve, 10));
				running--;
				return { content: [{ type: "text", text: id }], details: {} };
			},
		};
		const calls = Array.from({ length: 11 }, (_, index) => ({ ...fauxToolCall("read", {}), id: `call-${index}` }));
		const batch = await executeToolCalls(
			{ systemPrompt: "", messages: [], tools: [tool] },
			fauxAssistantMessage(calls),
			{ model, convertToLlm: () => [], toolExecution },
			undefined,
			() => {},
		);
		expect(peak).toBe(4);
		expect(batch.messages.map((message) => message.toolCallId)).toEqual(calls.map((call) => call.id));
	});
	it.each([undefined, "sequential"] as const)("serializes unknown/effectful batches (%s)", async (executionMode) => {
		const trace: string[] = [];
		const tool: AgentTool = {
			name: "write",
			label: "Write",
			description: "Mutates fixture",
			parameters: Type.Object({}),
			executionMode,
			execute: async (id) => {
				trace.push(`${id}:start`);
				await Promise.resolve();
				trace.push(`${id}:end`);
				return { content: [], details: {} };
			},
		};
		await executeToolCalls(
			{ systemPrompt: "", messages: [], tools: [tool] },
			fauxAssistantMessage([
				{ ...fauxToolCall("write", {}), id: "a" },
				{ ...fauxToolCall("write", {}), id: "b" },
			]),
			{ model, convertToLlm: () => [], toolExecution: "auto" },
			undefined,
			() => {},
		);
		expect(trace).toEqual(["a:start", "a:end", "b:start", "b:end"]);
	});
	it("does not start queued reads after cancellation", async () => {
		const controller = new AbortController();
		let executions = 0;
		const tool: AgentTool = {
			name: "read",
			label: "Read",
			description: "Read fixture",
			parameters: Type.Object({}),
			executionMode: "parallel",
			execute: async () => {
				executions++;
				controller.abort();
				return { content: [], details: {} };
			},
		};
		const batch = await executeToolCalls(
			{ systemPrompt: "", messages: [], tools: [tool] },
			fauxAssistantMessage(Array.from({ length: 8 }, () => fauxToolCall("read", {}))),
			{ model, convertToLlm: () => [], toolExecution: "auto" },
			controller.signal,
			() => {},
		);
		expect(executions).toBe(1);
		expect(batch.messages.filter((message) => message.isError)).toHaveLength(7);
	});
});
