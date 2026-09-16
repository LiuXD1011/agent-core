import { createAssistantMessageEventStream, Type } from "@liuxuedeng/agent-core-ai";
import { complete, getModel, getProviders, streamSimple } from "@liuxuedeng/agent-core-ai/compat";
import { Agent, agentLoop, streamProxy } from "@liuxuedeng/agent-core-agent";

// Exercise the supported browser-safe core, not the retired platform API.
const model = getModel("google", "gemini-2.5-flash");
const schema = Type.Object({ prompt: Type.String() });
const stream = createAssistantMessageEventStream();
const agent = new Agent({ initialState: { model }, streamFn: streamSimple });
agent.steer({ role: "user", content: [{ type: "text", text: "queued" }], timestamp: 0 });
console.log(model.id, getProviders().length, typeof complete, schema.type,
 typeof stream.push, agent.hasQueuedMessages(), typeof agentLoop, typeof streamProxy);
