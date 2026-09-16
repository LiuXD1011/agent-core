/**
 * Model request assembly: the fixed pipeline between "transcript so far" and
 * one provider request.
 *
 * 1. apply the configured context transform (AgentMessage[] → AgentMessage[]),
 * 2. convert to provider-compatible messages (AgentMessage[] → Message[]),
 * 3. assemble the LLM context with the system prompt and tool set,
 * 4. resolve a fresh API key so expiring tokens are renewed per request.
 */

import type { Context, Message } from "@liuxuedeng/agent-core-ai";
import type { AgentContext, AgentLoopConfig } from "../types.ts";

export async function buildModelRequest(
	context: AgentContext,
	config: AgentLoopConfig,
	signal: AbortSignal | undefined,
): Promise<Context & { apiKey?: string }> {
	let messages = context.messages;
	if (config.transformContext) {
		messages = await config.transformContext(messages, signal);
	}

	const llmMessages: Message[] = await config.convertToLlm(messages);

	const resolvedApiKey =
		(config.getApiKey ? await config.getApiKey(config.model.provider) : undefined) || config.apiKey;

	const llmContext: Context = {
		systemPrompt: context.systemPrompt,
		messages: llmMessages,
		tools: context.tools,
	};
	return { ...llmContext, apiKey: resolvedApiKey };
}
