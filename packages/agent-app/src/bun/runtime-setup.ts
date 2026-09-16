import { bedrockProviderModule } from "@liuxuedeng/agent-core-ai/bedrock-provider";
import { registerBunOAuthFlows } from "@liuxuedeng/agent-core-ai/bun-oauth";
import { setBedrockProviderModule } from "@liuxuedeng/agent-core-ai/compat";
import { APP_NAME } from "../config.ts";

process.title = APP_NAME;
process.emitWarning = (() => {}) as typeof process.emitWarning;
registerBunOAuthFlows();
setBedrockProviderModule(bedrockProviderModule);
