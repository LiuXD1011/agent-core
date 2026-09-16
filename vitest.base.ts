import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export const workspaceSourcePaths = {
	aiIndex: fileURLToPath(new URL("./packages/ai/src/index.ts", import.meta.url)),
	aiCompat: fileURLToPath(new URL("./packages/ai/src/compat.ts", import.meta.url)),
	aiOAuth: fileURLToPath(new URL("./packages/ai/src/oauth.ts", import.meta.url)),
	aiProviders: fileURLToPath(new URL("./packages/ai/src/providers", import.meta.url)),
	agentIndex: fileURLToPath(new URL("./packages/agent/src/index.ts", import.meta.url)),
	codingAgentIndex: fileURLToPath(new URL("./packages/agent-app/src/index.ts", import.meta.url)),
	tuiIndex: fileURLToPath(new URL("./packages/tui/src/index.ts", import.meta.url)),
} as const;

export default defineConfig({
	resolve: {
		alias: [
			{ find: /^@liuxuedeng\/agent-core-ai$/, replacement: workspaceSourcePaths.aiIndex },
			{ find: /^@liuxuedeng\/agent-core-ai\/compat$/, replacement: workspaceSourcePaths.aiCompat },
			{ find: /^@liuxuedeng\/agent-core-ai\/oauth$/, replacement: workspaceSourcePaths.aiOAuth },
			{
				find: /^@liuxuedeng\/agent-core-ai\/providers\/(.+)$/,
				replacement: `${workspaceSourcePaths.aiProviders}/$1.ts`,
			},
			{ find: /^@liuxuedeng\/agent-core-agent$/, replacement: workspaceSourcePaths.agentIndex },
			{ find: /^@liuxuedeng\/agent-core-tui$/, replacement: workspaceSourcePaths.tuiIndex },
		],
	},
});
