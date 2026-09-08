import { fileURLToPath } from "node:url";
import { defineConfig } from "vitest/config";

const telemetrySrcIndex = fileURLToPath(new URL("../telemetry/src/index.ts", import.meta.url));
const aiSrcIndex = fileURLToPath(new URL("../ai/src/index.ts", import.meta.url));
const aiSrcCompat = fileURLToPath(new URL("../ai/src/compat.ts", import.meta.url));

export default defineConfig({
	test: {
		environment: "node",
		benchmark: {
			include: ["benchmark/session/**/*.bench.ts"],
			reporters: ["verbose"],
		},
	},
	resolve: {
		alias: [
			{ find: /^@liuxuedeng\/pi-core-telemetry$/, replacement: telemetrySrcIndex },
			{ find: /^@liuxuedeng\/pi-core-ai$/, replacement: aiSrcIndex },
			{ find: /^@liuxuedeng\/pi-core-ai\/compat$/, replacement: aiSrcCompat },
		],
	},
});
