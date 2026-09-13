import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

export const workspaceSourcePaths = {
	chordIndex: fileURLToPath(new URL("./packages/chord/src/index.ts", import.meta.url)),
	chordContext: fileURLToPath(new URL("./packages/chord/src/context/index.ts", import.meta.url)),
	chordDelta: fileURLToPath(new URL("./packages/chord/src/delta/index.ts", import.meta.url)),
	chordBundler: fileURLToPath(new URL("./packages/chord/src/bundler.ts", import.meta.url)),
	chordNode: fileURLToPath(new URL("./packages/chord/src/node.ts", import.meta.url)),
	telemetryIndex: fileURLToPath(new URL("./packages/telemetry/src/index.ts", import.meta.url)),
	telemetryTesting: fileURLToPath(new URL("./packages/telemetry/src/testing/index.ts", import.meta.url)),
	aiIndex: fileURLToPath(new URL("./packages/ai/src/index.ts", import.meta.url)),
	aiCompat: fileURLToPath(new URL("./packages/ai/src/compat.ts", import.meta.url)),
	aiOAuth: fileURLToPath(new URL("./packages/ai/src/oauth.ts", import.meta.url)),
	aiProviders: fileURLToPath(new URL("./packages/ai/src/providers", import.meta.url)),
	agentIndex: fileURLToPath(new URL("./packages/agent/src/index.ts", import.meta.url)),
	agentNode: fileURLToPath(new URL("./packages/agent/src/node.ts", import.meta.url)),
	protocolIndex: fileURLToPath(new URL("./packages/protocol/src/index.ts", import.meta.url)),
	clientIndex: fileURLToPath(new URL("./packages/client/src/index.ts", import.meta.url)),
	clientUnix: fileURLToPath(new URL("./packages/client/src/unix.ts", import.meta.url)),
	serverIndex: fileURLToPath(new URL("./packages/server/src/index.ts", import.meta.url)),
	serverUnix: fileURLToPath(new URL("./packages/server/src/transports/unix/index.ts", import.meta.url)),
	codingAgentIndex: fileURLToPath(new URL("./packages/coding-agent/src/index.ts", import.meta.url)),
	tuiIndex: fileURLToPath(new URL("./packages/tui/src/index.ts", import.meta.url)),
} as const;

export default defineConfig({
	resolve: {
		alias: [
			{ find: /^@liuxuedeng\/agent-core-chord$/, replacement: workspaceSourcePaths.chordIndex },
			{ find: /^@liuxuedeng\/agent-core-chord\/context$/, replacement: workspaceSourcePaths.chordContext },
			{ find: /^@liuxuedeng\/agent-core-chord\/delta$/, replacement: workspaceSourcePaths.chordDelta },
			{ find: /^@liuxuedeng\/agent-core-chord\/bundler$/, replacement: workspaceSourcePaths.chordBundler },
			{ find: /^@liuxuedeng\/agent-core-chord\/node$/, replacement: workspaceSourcePaths.chordNode },
			{ find: /^@liuxuedeng\/agent-core-telemetry$/, replacement: workspaceSourcePaths.telemetryIndex },
			{ find: /^@liuxuedeng\/agent-core-telemetry\/testing$/, replacement: workspaceSourcePaths.telemetryTesting },
			{ find: /^@liuxuedeng\/agent-core-ai$/, replacement: workspaceSourcePaths.aiIndex },
			{ find: /^@liuxuedeng\/agent-core-ai\/compat$/, replacement: workspaceSourcePaths.aiCompat },
			{ find: /^@liuxuedeng\/agent-core-ai\/oauth$/, replacement: workspaceSourcePaths.aiOAuth },
			{
				find: /^@liuxuedeng\/agent-core-ai\/providers\/(.+)$/,
				replacement: `${workspaceSourcePaths.aiProviders}/$1.ts`,
			},
			{ find: /^@liuxuedeng\/agent-core-agent$/, replacement: workspaceSourcePaths.agentIndex },
			{ find: /^@liuxuedeng\/agent-core-agent\/node$/, replacement: workspaceSourcePaths.agentNode },
			{ find: /^@liuxuedeng\/agent-core-protocol$/, replacement: workspaceSourcePaths.protocolIndex },
			{ find: /^@liuxuedeng\/agent-core-client$/, replacement: workspaceSourcePaths.clientIndex },
			{ find: /^@liuxuedeng\/agent-core-client\/unix$/, replacement: workspaceSourcePaths.clientUnix },
			{ find: /^@liuxuedeng\/agent-core-server$/, replacement: workspaceSourcePaths.serverIndex },
			{ find: /^@liuxuedeng\/agent-core-server\/unix$/, replacement: workspaceSourcePaths.serverUnix },
			{ find: /^@liuxuedeng\/agent-core-tui$/, replacement: workspaceSourcePaths.tuiIndex },
		],
	},
});
