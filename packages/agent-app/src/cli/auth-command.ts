import type { AuthResult } from "@liuxuedeng/agent-core-ai";
import { APP_NAME } from "../config.ts";
import type { Args } from "./args.ts";

export type AuthCommandKind = "check" | "api_key" | "bearer_token";

export interface AuthCommand {
	kind: AuthCommandKind;
	args: string[];
	json: boolean;
	credentials: boolean;
	noRefresh: boolean;
	minExpiryMs?: number;
}

export class AuthCommandError extends Error {}

const AUTH_COMMAND_USAGE: Record<AuthCommandKind, string> = {
	check: `${APP_NAME} auth check --provider <provider> [--json] [--credentials] [--no-refresh]`,
	api_key: `${APP_NAME} auth print-api-key --provider <provider> [--model <model>]`,
	bearer_token: `${APP_NAME} auth print-bearer-token --provider <provider> [--model <model>] [--min-expiry <duration>]`,
};

export function getAuthCommandName(kind: AuthCommandKind): string {
	return kind === "check" ? "auth check" : kind === "api_key" ? "auth print-api-key" : "auth print-bearer-token";
}

export function getAuthCommandUsage(kind: AuthCommandKind): string {
	return AUTH_COMMAND_USAGE[kind];
}

export function isAuthCommandHelp(args: string[]): boolean {
	return (
		args[0] === "auth" &&
		(args[1] === undefined || args[1] === "help" || args.includes("--help") || args.includes("-h"))
	);
}

export function printAuthCommandHelp(): void {
	console.log(`用法:
  ${APP_NAME} auth print-api-key [--provider <服务商>] [--model <模型>]
  ${APP_NAME} auth print-bearer-token [--provider <服务商>] [--model <模型>] [--min-expiry <时长>]
  ${APP_NAME} auth check [--provider <服务商>] [--model <模型>] [--json] [--credentials] [--no-refresh]

认证命令至少需要 --provider 或 --model 之一。默认会刷新过期的 OAuth 凭据；--no-refresh 可阻止刷新。--credentials 会输出凭据，或将其包含在 JSON 输出中。`);
}

export function parseAuthCommand(args: string[]): AuthCommand | undefined {
	if (args[0] !== "auth") return undefined;

	const kind =
		args[1] === "check"
			? "check"
			: args[1] === "print-api-key"
				? "api_key"
				: args[1] === "print-bearer-token"
					? "bearer_token"
					: undefined;
	if (!kind) {
		throw new AuthCommandError(
			`未知认证命令 "${args[1] ?? ""}"。请使用 "${APP_NAME} auth print-api-key"、"${APP_NAME} auth print-bearer-token" 或 "${APP_NAME} auth check"。`,
		);
	}

	const commandArgs: string[] = [];
	let json = false;
	let credentials = false;
	let noRefresh = false;
	let minExpiryMs: number | undefined;
	for (let index = 2; index < args.length; index++) {
		const arg = args[index];
		if (arg === "--min-expiry") {
			if (kind !== "bearer_token") throw new AuthCommandError("--min-expiry 仅 print-bearer-token 支持");
			const value = args[++index];
			const match = value ? /^(\d+)(ms|s|m|h)$/iu.exec(value) : undefined;
			if (!match) throw new AuthCommandError("--min-expiry 需要时长格式，例如 30m 或 1h");
			const amount = Number(match[1]);
			const unit = match[2];
			minExpiryMs = amount * (unit === "ms" ? 1 : unit === "s" ? 1_000 : unit === "m" ? 60_000 : 3_600_000);
			continue;
		}
		if (arg === "--json" || arg === "--credentials" || arg === "--no-refresh") {
			if (kind !== "check") throw new AuthCommandError(`${arg} 仅 auth check 支持`);
			if (arg === "--json") json = true;
			else if (arg === "--credentials") credentials = true;
			else noRefresh = true;
			continue;
		}
		commandArgs.push(arg);
	}

	return minExpiryMs === undefined
		? { kind, args: commandArgs, json, credentials, noRefresh }
		: { kind, args: commandArgs, json, credentials, noRefresh, minExpiryMs };
}

export function validateAuthCommandArgs(args: Args, kind: AuthCommandKind): { provider?: string; model?: string } {
	const provider = args.provider?.trim() || undefined;
	const model = args.model?.trim() || undefined;
	if (args.unknownFlags.size > 0) {
		const option = args.unknownFlags.keys().next().value;
		throw new AuthCommandError(`"${getAuthCommandName(kind)}" 存在未知选项 --${option}。`);
	}
	if (args.apiKey !== undefined || args.messages.length > 0 || args.fileArgs.length > 0) {
		throw new AuthCommandError("认证命令只接受 --provider 和 --model");
	}
	if (kind === "check") {
		if (!provider && !model) {
			throw new AuthCommandError("认证检查需要 --provider <服务商> 或 --model <模型>");
		}
		return { provider, model };
	}
	if (!provider && !model) {
		throw new AuthCommandError("打印凭据需要 --provider <服务商> 或 --model <模型>");
	}
	return { provider, model };
}

export function getAuthCredential(auth: AuthResult | undefined): string | undefined {
	if (auth?.auth.apiKey) return auth.auth.apiKey;
	const authorization = Object.entries(auth?.auth.headers ?? {}).find(
		([name]) => name.toLowerCase() === "authorization",
	)?.[1];
	return typeof authorization === "string" ? /^Bearer\s+(.+)$/iu.exec(authorization)?.[1] : undefined;
}
