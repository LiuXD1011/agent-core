import { Agent } from "@liuxuedeng/agent-core-agent";
import { createModels } from "@liuxuedeng/agent-core-ai";
import { anthropicProvider } from "@liuxuedeng/agent-core-ai/providers/anthropic";

const models = createModels();
models.setProvider(anthropicProvider());
const model = models.getModel("anthropic", "claude-sonnet-4-5");
if (!model) throw new Error("Anthropic smoke-test model not found");

export const agent = new Agent({
	initialState: { model },
	streamFn: models.streamSimple.bind(models),
});
