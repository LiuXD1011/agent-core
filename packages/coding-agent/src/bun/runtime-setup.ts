import { bedrockProviderModule } from "@liuxuedeng/pi-core-ai/bedrock-provider";
import { registerBunOAuthFlows } from "@liuxuedeng/pi-core-ai/bun-oauth";
import { setBedrockProviderModule } from "@liuxuedeng/pi-core-ai/compat";
import { APP_NAME } from "../config.ts";

process.title = APP_NAME;
process.emitWarning = (() => {}) as typeof process.emitWarning;
registerBunOAuthFlows();
setBedrockProviderModule(bedrockProviderModule);
