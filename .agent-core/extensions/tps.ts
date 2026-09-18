import type { AssistantMessage } from "@liuxuedeng/agent-core-ai";
import type { ExtensionAPI } from "@liuxuedeng/agent-core";

function isAssistantMessage(message: unknown): message is AssistantMessage {
	if (!message || typeof message !== "object") return false;
	const role = (message as { role?: unknown }).role;
	return role === "assistant";
}

export default function (pi: ExtensionAPI) {
	let agentStartMs: number | null = null;

	pi.on("agent_start", () => {
		agentStartMs = Date.now();
	});

	pi.on("agent_end", (event, ctx) => {
		if (!ctx.hasUI) return;
		if (agentStartMs === null) return;

		const elapsedMs = Date.now() - agentStartMs;
		agentStartMs = null;
		if (elapsedMs <= 0) return;

		let output = 0;

		for (const message of event.messages) {
			if (!isAssistantMessage(message)) continue;
			output += message.usage.output || 0;
		}

		if (output <= 0) return;

		const elapsedSeconds = elapsedMs / 1000;
		const tokensPerSecond = output / elapsedSeconds;
		const message = `耗时 ${elapsedSeconds.toFixed(1)} 秒 · 平均输出 ${tokensPerSecond.toFixed(1)} Token/秒`;
		ctx.ui.notify(message, "info");
	});
}
