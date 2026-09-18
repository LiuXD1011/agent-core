import type { Api, CredentialInfo, Model } from "@liuxuedeng/agent-core-ai";
import { resolveCliModel } from "../app/model-resolver.ts";
import type { ModelRuntime } from "../app/model-runtime.ts";
import type { Args } from "./args.ts";
import { AuthCommandError, type AuthCommandKind, getAuthCredential, validateAuthCommandArgs } from "./auth-command.ts";

const DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS = 30 * 60_000;

type CredentialPrintKind = Exclude<AuthCommandKind, "check">;

/**
 * Resolve one configured provider credential.
 *
 * This intentionally calls ModelRuntime.getAuth(), which refreshes and persists
 * OAuth credentials with less than five minutes remaining through the normal request-auth path.
 */
export async function resolveCredentialForPrint(
	args: Args,
	modelRuntime: ModelRuntime,
	kind: CredentialPrintKind,
	minExpiryMs?: number,
	signal?: AbortSignal,
): Promise<string> {
	const { provider: cliProvider, model: cliModel } = validateAuthCommandArgs(args, kind);
	const credentialTypes = new Map<string, CredentialInfo["type"]>(
		(await modelRuntime.listCredentials({ signal })).map((credential) => [credential.providerId, credential.type]),
	);
	const providers: Array<{ id: string; model?: Model<Api> }> = [];
	if (cliProvider) {
		const provider = modelRuntime.getProvider(cliProvider);
		if (!provider) {
			throw new AuthCommandError(`未知服务商 "${cliProvider}"。使用 --list-models 查看可用服务商。`);
		}
		if (cliModel) {
			const resolved = resolveCliModel({ cliProvider: provider.id, cliModel, modelRuntime });
			if (resolved.error || !resolved.model) {
				throw new AuthCommandError(resolved.error ?? "无法解析所请求的服务商/模型");
			}
			providers.push({ id: provider.id, model: resolved.model });
		} else {
			providers.push({ id: provider.id });
		}
	} else {
		for (const provider of modelRuntime.getProviders()) {
			if (!credentialTypes.has(provider.id)) continue;
			const resolved = resolveCliModel({ cliProvider: provider.id, cliModel: cliModel!, modelRuntime });
			if (
				resolved.model &&
				!resolved.error &&
				modelRuntime
					.getModels()
					.some((model) => model.provider === resolved.model?.provider && model.id === resolved.model.id)
			) {
				providers.push({ id: provider.id, model: resolved.model });
			}
		}
		if (providers.length === 0) {
			throw new AuthCommandError(`未找到模型 "${cliModel}"。使用 --list-models 查看可用模型。`);
		}
	}

	const credentials: Array<{ providerId: string; value: string }> = [];
	for (const provider of providers) {
		const type = credentialTypes.get(provider.id);
		if (kind === "api_key" && type === "oauth") continue;
		if (kind === "bearer_token" && type !== "oauth") continue;
		const authOptions = {
			...(kind === "bearer_token" ? { minOAuthValidityMs: minExpiryMs ?? DEFAULT_BEARER_TOKEN_MIN_EXPIRY_MS } : {}),
			signal,
		};
		const auth = provider.model
			? await modelRuntime.getAuth(provider.model, authOptions)
			: await modelRuntime.getAuth(provider.id, authOptions);
		const value = getAuthCredential(auth);
		if (value) credentials.push({ providerId: provider.id, value });
	}

	if (credentials.length === 1) return credentials[0].value;
	if (credentials.length === 0) {
		const providerId = providers[0]?.id;
		const type = providerId ? credentialTypes.get(providerId) : undefined;
		if (cliProvider && kind === "api_key" && type === "oauth") {
			throw new AuthCommandError(`服务商 "${providerId}" 配置的是 OAuth，而非 API 密钥`);
		}
		if (cliProvider && kind === "bearer_token" && type !== "oauth") {
			throw new AuthCommandError(`服务商 "${providerId}" 未配置 OAuth Bearer Token`);
		}
		throw new AuthCommandError(`未配置可用的${kind === "api_key" ? " API 密钥" : " OAuth Bearer Token"}`);
	}
	throw new AuthCommandError(
		`匹配到多个已配置的服务商（${credentials.map(({ providerId }) => providerId).join("、")}）。请指定 --provider。`,
	);
}
