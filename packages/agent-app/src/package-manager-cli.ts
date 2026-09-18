import {
	existsSync,
	mkdirSync,
	mkdtempSync,
	readdirSync,
	readFileSync,
	renameSync,
	rmSync,
	writeFileSync,
} from "node:fs";
import { join, resolve } from "node:path";
import { Markdown, type MarkdownTheme } from "@liuxuedeng/agent-core-tui";
import chalk from "chalk";
import lockfile from "proper-lockfile";
import { ModelRuntime } from "./app/model-runtime.ts";
import { DefaultPackageManager } from "./app/package-manager.ts";
import { type AppMode, resolveProjectTrusted } from "./app/project-trust.ts";
import { DefaultResourceLoader } from "./app/resource-loader.ts";
import { SettingsManager } from "./app/settings-manager.ts";
import { hasTrustRequiringProjectResources, ProjectTrustStore } from "./app/trust-manager.ts";
import { selectConfig } from "./cli/config-selector.ts";
import { createProjectTrustContext } from "./cli/project-trust.ts";
import {
	APP_NAME,
	CONFIG_DIR_NAME,
	detectInstallMethod,
	getAgentDir,
	getPackageDir,
	getSelfUpdateCommand,
	getSelfUpdateUnavailableInstruction,
	PACKAGE_NAME,
	type SelfUpdateCommand,
	type SelfUpdatePackageTarget,
	VERSION,
} from "./config.ts";
import type { InlineExtension } from "./extensions/types.ts";
import { spawnProcess, spawnProcessSync, waitForChildProcess } from "./utils/child-process.ts";
import { canonicalizePath, getCwdRelativePath } from "./utils/paths.ts";
import { getPiUserAgent } from "./utils/pi-user-agent.ts";
import { formatVersionCheckError, getLatestPiRelease, isNewerPackageVersion } from "./utils/version-check.ts";
import {
	cleanupWindowsSelfUpdateQuarantine,
	quarantineWindowsNativeDependencies,
} from "./utils/windows-self-update.ts";

export type PackageCommand = "install" | "remove" | "update" | "list";

type UpdateTarget = { type: "all" } | { type: "self" } | { type: "extensions"; source?: string } | { type: "models" };

const MANAGED_INSTALL_MARKER = "managed-install.json";
const MANAGED_RELEASE_VERSION_RE = /^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/;

function getActiveManagedInstallRoot(): string | undefined {
	const configuredRoot = process.env.AGENT_CORE_MANAGED_INSTALL_ROOT?.trim();
	if (!configuredRoot) return undefined;

	const managedRoot = resolve(configuredRoot);
	const releasesDir = canonicalizePath(join(managedRoot, "releases"));
	// The launcher environment is inherited by child processes. Do not classify a
	// source checkout or another Pi installation launched from managed Pi as managed.
	if (getCwdRelativePath(canonicalizePath(getPackageDir()), releasesDir) === undefined) return undefined;

	const markerPath = join(managedRoot, MANAGED_INSTALL_MARKER);
	try {
		const marker = JSON.parse(readFileSync(markerPath, "utf8")) as {
			kind?: unknown;
			layout?: unknown;
			schemaVersion?: unknown;
		};
		if (marker.kind !== "pi-managed-install" || marker.schemaVersion !== 1 || marker.layout !== "releases-v1") {
			throw new Error();
		}
	} catch {
		throw new Error(`托管安装标记缺失或无效：${markerPath}`);
	}

	return managedRoot;
}

async function fetchInstallerArtifact(url: string, label: string): Promise<string> {
	const response = await fetch(url, { headers: { "User-Agent": getPiUserAgent(VERSION) } });
	if (!response.ok) {
		throw new Error(`无法从 ${url} 下载托管安装器 ${label}：HTTP ${response.status}`);
	}
	return await response.text();
}

async function runManagedNpmCi(stageDir: string): Promise<void> {
	const args = [
		"ci",
		"--ignore-scripts",
		"--min-release-age=0",
		"--omit=dev",
		"--include=optional",
		"--no-fund",
		"--no-audit",
		"--loglevel=error",
		"--progress=false",
	];
	const code = await waitForChildProcess(spawnProcess("npm", args, { cwd: stageDir, stdio: "inherit" }));
	if (code !== 0) throw new Error(`npm ${args.join(" ")} 以退出码 ${code ?? "未知"} 退出`);
}

function verifyManagedRelease(releaseDir: string, expectedVersion: string): void {
	const binPath = join(
		releaseDir,
		"node_modules",
		".bin",
		process.platform === "win32" ? `${APP_NAME}.cmd` : APP_NAME,
	);
	const result = spawnProcessSync(binPath, ["--version"], {
		encoding: "utf8",
		stdio: ["ignore", "pipe", "pipe"],
	});
	if (result.error || result.status !== 0) {
		const reason = result.error?.message || result.stderr.trim() || `退出码 ${result.status ?? "未知"}`;
		throw new Error(`无法验证托管安装的 ${APP_NAME} ${expectedVersion}：${reason}`);
	}
	const installedVersion = result.stdout.trim();
	if (installedVersion !== expectedVersion) {
		throw new Error(`托管安装的 ${APP_NAME} 冒烟测试返回版本 ${installedVersion}；期望 ${expectedVersion}。`);
	}
}

function activateManagedRelease(managedRoot: string, version: string): void {
	const currentPath = join(managedRoot, "current-version");
	const temporaryPath = join(managedRoot, `current-version.tmp.${process.pid}-${Date.now()}`);
	try {
		writeFileSync(temporaryPath, `${version}\n`);
		renameSync(temporaryPath, currentPath);
	} finally {
		rmSync(temporaryPath, { force: true });
	}
}

function cleanupManagedStaging(managedRoot: string): void {
	const stagingRoot = join(managedRoot, "staging");
	try {
		for (const entry of readdirSync(stagingRoot)) {
			if (entry.startsWith("update-")) {
				rmSync(join(stagingRoot, entry), { force: true, recursive: true });
			}
		}
	} catch {
		// The staging directory does not exist yet or is not writable.
	}
}

export function cleanupManagedInstall(): void {
	let managedRoot: string | undefined;
	try {
		managedRoot = getActiveManagedInstallRoot();
	} catch {
		return;
	}
	if (!managedRoot) return;

	try {
		const releaseLock = lockfile.lockSync(join(managedRoot, "update"), { realpath: false });
		try {
			cleanupManagedStaging(managedRoot);
		} finally {
			releaseLock();
		}
	} catch {
		// A live update owns the staging directory, or cleanup is unavailable.
	}
}

export async function runManagedSelfUpdate(managedRoot: string, version: string): Promise<void> {
	if (!MANAGED_RELEASE_VERSION_RE.test(version)) {
		throw new Error(`托管发行版本无效：${version}`);
	}

	let releaseLock: () => Promise<void>;
	try {
		releaseLock = await lockfile.lock(join(managedRoot, "update"), { realpath: false });
	} catch (error: unknown) {
		if (error instanceof Error && "code" in error && error.code === "ELOCKED") {
			throw new Error(`另一个 ${APP_NAME} 托管更新正在进行中。`);
		}
		throw error;
	}

	let stageDir: string | undefined;
	try {
		// Managed updates need an explicitly configured Agent Core install source;
		// the upstream installer manifest is never used implicitly. Terminate
		// before staging or downloading anything.
		const configuredInstallerApiBase = process.env.AGENT_CORE_INSTALLER_API_BASE?.trim();
		if (!configuredInstallerApiBase) {
			throw new Error(
				`未为 ${APP_NAME} 配置托管更新源。请设置 AGENT_CORE_INSTALLER_API_BASE 指向 Agent Core 发行源，或使用以下命令升级：npm install -g ${PACKAGE_NAME}@latest`,
			);
		}
		const installerApiBase = configuredInstallerApiBase.replace(/\/+$/, "");
		cleanupManagedStaging(managedRoot);
		const releaseUrl = `${installerApiBase}/${encodeURIComponent(version)}`;
		const stagingRoot = join(managedRoot, "staging");
		const releasesRoot = join(managedRoot, "releases");
		mkdirSync(releasesRoot, { recursive: true });
		const releaseDir = join(releasesRoot, version);
		if (existsSync(releaseDir)) {
			verifyManagedRelease(releaseDir, version);
			activateManagedRelease(managedRoot, version);
			return;
		}

		mkdirSync(stagingRoot, { recursive: true });
		stageDir = mkdtempSync(join(stagingRoot, "update-"));
		const [packageJsonContent, packageLockContent] = await Promise.all([
			fetchInstallerArtifact(`${releaseUrl}/package.json`, "package.json"),
			fetchInstallerArtifact(`${releaseUrl}/package-lock.json`, "package-lock.json"),
		]);
		// Never activate a manifest that installs a different product.
		const stagedPackage = JSON.parse(packageJsonContent) as { name?: unknown };
		if (stagedPackage.name !== PACKAGE_NAME) {
			throw new Error(
				`托管更新源提供的是 ${typeof stagedPackage.name === "string" ? stagedPackage.name : "未知包"}，而非 ${PACKAGE_NAME}。拒绝安装其他产品。`,
			);
		}
		writeFileSync(join(stageDir, "package.json"), packageJsonContent);
		writeFileSync(join(stageDir, "package-lock.json"), packageLockContent);

		await runManagedNpmCi(stageDir);
		verifyManagedRelease(stageDir, version);
		renameSync(stageDir, releaseDir);
		activateManagedRelease(managedRoot, version);
	} finally {
		if (stageDir) rmSync(stageDir, { force: true, recursive: true });
		await releaseLock();
	}
}

const SELF_UPDATE_NOTE_MARKDOWN_THEME: MarkdownTheme = {
	heading: (text) => chalk.bold(chalk.yellow(text)),
	link: (text) => chalk.cyan(text),
	linkUrl: (text) => chalk.dim(text),
	code: (text) => chalk.yellow(text),
	codeBlock: (text) => chalk.dim(text),
	codeBlockBorder: (text) => chalk.dim(text),
	quote: (text) => chalk.dim(text),
	quoteBorder: (text) => chalk.dim(text),
	hr: (text) => chalk.dim(text),
	listBullet: (text) => chalk.yellow(text),
	bold: (text) => chalk.bold(text),
	italic: (text) => chalk.italic(text),
	strikethrough: (text) => chalk.strikethrough(text),
	underline: (text) => chalk.underline(text),
};

interface PackageCommandOptions {
	command: PackageCommand;
	source?: string;
	updateTarget?: UpdateTarget;
	showExtensionsSkippedNote: boolean;
	local: boolean;
	force: boolean;
	projectTrustOverride?: boolean;
	help: boolean;
	invalidOption?: string;
	invalidArgument?: string;
	missingOptionValue?: string;
	conflictingOptions?: string;
}

function reportSettingsErrors(settingsManager: SettingsManager, context: string): void {
	const errors = settingsManager.drainErrors();
	for (const { scope, error } of errors) {
		console.error(chalk.yellow(`警告（${context}，${scope} 设置）：${error.message}`));
		if (error.stack) {
			console.error(chalk.dim(error.stack));
		}
	}
}

function getPackageCommandUsage(command: PackageCommand): string {
	switch (command) {
		case "install":
			return `${APP_NAME} install <source> [-l] [--approve|--no-approve]`;
		case "remove":
			return `${APP_NAME} remove <source> [-l] [--approve|--no-approve]`;
		case "update":
			return `${APP_NAME} update [source|self] [--self|--extensions|--models|--all] [--extension <source>] [--approve|--no-approve] [--force]`;
		case "list":
			return `${APP_NAME} list [--approve|--no-approve]`;
	}
}

const CONFIG_COMMAND_USAGE = `${APP_NAME} config [-l] [--approve|--no-approve]`;

function printConfigCommandHelp(): void {
	console.log(`${chalk.bold("用法:")}
  ${CONFIG_COMMAND_USAGE}

打开资源配置 TUI，启用或停用包资源。
不带 -l 时从全局设置（~/${CONFIG_DIR_NAME}/agent/settings.json）开始。
在 TUI 中按 Tab 可在全局与项目作用域之间切换。

选项:
  -l, --local       编辑项目覆盖配置（${CONFIG_DIR_NAME}/settings.json）
  -a, --approve     本次命令信任项目本地文件
  -na, --no-approve 本次命令忽略项目本地文件
`);
}

function printPackageCommandHelp(command: PackageCommand): void {
	switch (command) {
		case "install":
			console.log(`${chalk.bold("用法:")}
  ${getPackageCommandUsage("install")}

安装包并写入设置。

选项:
  -l, --local       安装到项目本地（${CONFIG_DIR_NAME}/settings.json）
  -a, --approve     本次命令信任项目本地文件
  -na, --no-approve 本次命令忽略项目本地文件

示例:
  ${APP_NAME} install npm:@foo/bar
  ${APP_NAME} install git:github.com/user/repo
  ${APP_NAME} install git:git@github.com:user/repo
  ${APP_NAME} install https://github.com/user/repo
  ${APP_NAME} install ssh://git@github.com/user/repo
  ${APP_NAME} install ./local/path
`);
			return;

		case "remove":
			console.log(`${chalk.bold("用法:")}
  ${getPackageCommandUsage("remove")}

移除包及其在设置中的源。
别名：${APP_NAME} uninstall <source> [-l]

选项:
  -l, --local       从项目设置移除（${CONFIG_DIR_NAME}/settings.json）
  -a, --approve     本次命令信任项目本地文件
  -na, --no-approve 本次命令忽略项目本地文件

示例:
  ${APP_NAME} remove npm:@foo/bar
  ${APP_NAME} uninstall npm:@foo/bar
`);
			return;

		case "update":
			console.log(`${chalk.bold("用法:")}
  ${getPackageCommandUsage("update")}

更新 agent-core、已安装的包或模型目录。

选项:
  --self                  只更新 agent-core（未指定目标时默认）
  --extensions            只更新已安装的包
  --models                只刷新模型目录
  --all                   更新 agent-core 和已安装的包
  --extension <source>    只更新一个包
  -a, --approve           本次命令信任项目本地文件
  -na, --no-approve       本次命令忽略项目本地文件
  --force                 即使已是最新版本也重新安装 agent-core

简写形式:
  ${APP_NAME} update                只更新 agent-core
  ${APP_NAME} update --all          更新 agent-core 和全部扩展
  ${APP_NAME} update --models       只刷新模型目录
  ${APP_NAME} update <source>       更新一个包
  ${APP_NAME} update agent-core     只更新 agent-core（self 和 pi 是别名）
`);
			return;

		case "list":
			console.log(`${chalk.bold("用法:")}
  ${getPackageCommandUsage("list")}

列出用户与项目设置中已安装的包。

选项:
  -a, --approve      本次命令信任项目本地文件
  -na, --no-approve  本次命令忽略项目本地文件
`);
			return;
	}
}

function parsePackageCommand(args: string[]): PackageCommandOptions | undefined {
	const [rawCommand, ...rest] = args;
	let command: PackageCommand | undefined;
	if (rawCommand === "uninstall") {
		command = "remove";
	} else if (rawCommand === "install" || rawCommand === "remove" || rawCommand === "update" || rawCommand === "list") {
		command = rawCommand;
	}
	if (!command) {
		return undefined;
	}

	let local = false;
	let force = false;
	let projectTrustOverride: boolean | undefined;
	let help = false;
	let invalidOption: string | undefined;
	let invalidArgument: string | undefined;
	let missingOptionValue: string | undefined;
	let conflictingOptions: string | undefined;
	let source: string | undefined;
	let selfFlag = false;
	let extensionsFlag = false;
	let modelsFlag = false;
	let allFlag = false;
	let extensionFlagSource: string | undefined;

	for (let index = 0; index < rest.length; index++) {
		const arg = rest[index];
		if (arg === "-h" || arg === "--help") {
			help = true;
			continue;
		}

		if (arg === "-l" || arg === "--local") {
			if (command === "install" || command === "remove") {
				local = true;
			} else {
				invalidOption = invalidOption ?? arg;
			}
			continue;
		}

		if (arg === "--self") {
			if (command === "update") {
				selfFlag = true;
			} else {
				invalidOption = invalidOption ?? arg;
			}
			continue;
		}

		if (arg === "--extensions") {
			if (command === "update") {
				extensionsFlag = true;
			} else {
				invalidOption = invalidOption ?? arg;
			}
			continue;
		}

		if (arg === "--models") {
			if (command === "update") {
				modelsFlag = true;
			} else {
				invalidOption = invalidOption ?? arg;
			}
			continue;
		}

		if (arg === "--all") {
			if (command === "update") {
				allFlag = true;
			} else {
				invalidOption = invalidOption ?? arg;
			}
			continue;
		}

		if (arg === "--approve" || arg === "-a") {
			projectTrustOverride = true;
			continue;
		}

		if (arg === "--no-approve" || arg === "-na") {
			projectTrustOverride = false;
			continue;
		}

		if (arg === "--force") {
			if (command === "update") {
				force = true;
			} else {
				invalidOption = invalidOption ?? arg;
			}
			continue;
		}

		if (arg === "--extension") {
			if (command !== "update") {
				invalidOption = invalidOption ?? arg;
				continue;
			}

			const value = rest[index + 1];
			if (!value || value.startsWith("-")) {
				missingOptionValue = missingOptionValue ?? arg;
			} else if (extensionFlagSource) {
				conflictingOptions = conflictingOptions ?? "--extension 只能提供一次";
				index++;
			} else {
				extensionFlagSource = value;
				index++;
			}
			continue;
		}

		if (arg.startsWith("-")) {
			invalidOption = invalidOption ?? arg;
			continue;
		}

		if (!source) {
			source = arg;
		} else {
			invalidArgument = invalidArgument ?? arg;
		}
	}

	let updateTarget: UpdateTarget | undefined;
	let showExtensionsSkippedNote = false;
	if (command === "update") {
		if (allFlag && (selfFlag || extensionsFlag || modelsFlag || extensionFlagSource)) {
			conflictingOptions =
				conflictingOptions ?? "--all 不能与 --self、--extensions、--models 或 --extension 同时使用";
		}
		if (allFlag && source) {
			conflictingOptions = conflictingOptions ?? "--all 不能与位置参数 source 同时使用";
		}

		if (modelsFlag) {
			if (selfFlag || extensionsFlag || allFlag || extensionFlagSource) {
				conflictingOptions =
					conflictingOptions ?? "--models 不能与 --self、--extensions、--all 或 --extension 同时使用";
			}
			if (source) {
				conflictingOptions = conflictingOptions ?? "--models 不能与位置参数 source 同时使用";
			}
			updateTarget = { type: "models" };
		} else if (extensionFlagSource) {
			if (selfFlag || extensionsFlag || allFlag) {
				conflictingOptions = conflictingOptions ?? "--extension 不能与 --self、--extensions 或 --all 同时使用";
			}
			if (source) {
				conflictingOptions = conflictingOptions ?? "--extension 不能与位置参数 source 同时使用";
			}
			updateTarget = { type: "extensions", source: extensionFlagSource };
		} else if (source) {
			const sourceIsSelf = source === "self" || source === "pi" || source === "agent-core";
			if (sourceIsSelf) {
				updateTarget = extensionsFlag ? { type: "all" } : { type: "self" };
			} else {
				if (extensionsFlag || selfFlag || allFlag) {
					conflictingOptions =
						conflictingOptions ?? "位置参数 update 目标不能与 --self、--extensions 或 --all 同时使用";
				}
				updateTarget = { type: "extensions", source };
			}
		} else if (allFlag) {
			updateTarget = { type: "all" };
		} else if (selfFlag && extensionsFlag) {
			updateTarget = { type: "all" };
		} else if (selfFlag) {
			updateTarget = { type: "self" };
		} else if (extensionsFlag) {
			updateTarget = { type: "extensions" };
		} else {
			updateTarget = { type: "self" };
			showExtensionsSkippedNote = true;
		}
	}

	return {
		command,
		source,
		updateTarget,
		showExtensionsSkippedNote,
		local,
		force,
		projectTrustOverride,
		help,
		invalidOption,
		invalidArgument,
		missingOptionValue,
		conflictingOptions,
	};
}

function updateTargetIncludesSelf(target: UpdateTarget): boolean {
	return target.type === "all" || target.type === "self";
}

function updateTargetIncludesExtensions(target: UpdateTarget): boolean {
	return target.type === "all" || target.type === "extensions";
}

async function refreshModelCatalogs(agentDir: string): Promise<void> {
	const controller = new AbortController();
	const timeout = setTimeout(() => controller.abort(), 15_000);
	try {
		const modelRuntime = await ModelRuntime.create({
			authPath: join(agentDir, "auth.json"),
			modelsPath: join(agentDir, "models.json"),
			allowModelNetwork: false,
			signal: controller.signal,
		});
		const result = await modelRuntime.refresh({
			allowNetwork: true,
			force: true,
			signal: controller.signal,
		});
		if (result.aborted) {
			throw new Error("模型目录刷新超时。");
		}
		if (result.errors.size > 0) {
			const details = Array.from(result.errors, ([provider, error]) => `${provider}: ${error.message}`).join("; ");
			throw new Error(`无法刷新模型目录：${details}`);
		}
	} finally {
		clearTimeout(timeout);
	}
	console.log(chalk.green("模型目录已刷新"));
}

function printSelfUpdateUnavailable(
	npmCommand?: string[],
	updatePackageTarget: SelfUpdatePackageTarget = PACKAGE_NAME,
): void {
	console.error(`错误：当前安装方式不支持 ${APP_NAME} 自更新。`);
	console.error(getSelfUpdateUnavailableInstruction(PACKAGE_NAME, npmCommand, updatePackageTarget));

	const entrypoint = process.argv[1];
	if (entrypoint) {
		console.error("");
		console.error(`${APP_NAME} 可执行文件位置：${entrypoint}`);
	}
}

function printSelfUpdateFallback(command: SelfUpdateCommand): void {
	console.error(chalk.dim(`如果持续失败，请自行执行：${command.display}`));
}

function printPnpmSelfUpdateMetadataHint(): void {
	console.error(chalk.yellow("如果 pnpm 报告缺少包版本，可能是其缓存的注册表元数据过期。"));
	console.error(chalk.yellow(`执行 \`pnpm store prune\` 后重试 \`${APP_NAME} update --self\`。`));
}

function printSelfUpdateNote(note: string): void {
	const trimmedNote = note.trim();
	if (!trimmedNote) {
		return;
	}

	console.log();
	console.log(chalk.bold(chalk.yellow("更新说明")));
	try {
		const width = Math.max(20, process.stdout.columns ?? 80);
		const renderedLines = new Markdown(trimmedNote, 0, 0, SELF_UPDATE_NOTE_MARKDOWN_THEME)
			.render(width)
			.map((line) => line.trimEnd());
		console.log(renderedLines.join("\n"));
	} catch {
		console.log(trimmedNote);
	}
	console.log();
}

interface SelfUpdatePlan {
	packageName: string;
	installSpec: string;
	version: string;
	shouldRun: boolean;
	note?: string;
}

export async function getSelfUpdatePlan(force: boolean): Promise<SelfUpdatePlan> {
	let latestRelease: Awaited<ReturnType<typeof getLatestPiRelease>>;
	try {
		latestRelease = await getLatestPiRelease(VERSION, { retry: true });
	} catch (error: unknown) {
		throw new Error(`无法确定 ${APP_NAME} 最新版本：${formatVersionCheckError(error)}`, {
			cause: error,
		});
	}
	if (!latestRelease) {
		throw new Error(`${APP_NAME} 未配置自更新版本源。请使用以下命令升级：npm install -g ${PACKAGE_NAME}@latest`);
	}

	// A version feed must never silently switch the installed product.
	const feedPackageName = latestRelease.packageName;
	if (feedPackageName && feedPackageName !== PACKAGE_NAME) {
		throw new Error(
			`配置的版本源提供的是 ${feedPackageName}，而非 ${PACKAGE_NAME}。拒绝安装其他产品。请使用以下命令升级：npm install -g ${PACKAGE_NAME}@latest`,
		);
	}

	const installSpec = `${PACKAGE_NAME}@${latestRelease.version}`;
	if (force || isNewerPackageVersion(latestRelease.version, VERSION)) {
		return {
			packageName: PACKAGE_NAME,
			installSpec,
			version: latestRelease.version,
			...(latestRelease.note ? { note: latestRelease.note } : {}),
			shouldRun: true,
		};
	}

	console.log(chalk.green(`${APP_NAME} 已是最新版本（v${VERSION}）`));
	return { packageName: PACKAGE_NAME, installSpec, version: latestRelease.version, shouldRun: false };
}

async function runSelfUpdate(command: SelfUpdateCommand): Promise<void> {
	console.log(chalk.dim(`正在使用 ${command.display} 更新 ${APP_NAME}...`));
	for (const step of command.steps ?? [command]) {
		await new Promise<void>((resolve, reject) => {
			const child = spawnProcess(step.command, step.args, {
				stdio: "inherit",
			});
			child.on("error", (error) => {
				reject(error);
			});
			child.on("close", (code, signal) => {
				if (code === 0) {
					resolve();
				} else if (signal) {
					reject(new Error(`${step.display} 被信号 ${signal} 终止`));
				} else {
					reject(new Error(`${step.display} 以退出码 ${code ?? "未知"} 退出`));
				}
			});
		});
	}
}

function prepareWindowsNpmSelfUpdate(): void {
	if (process.platform !== "win32") {
		return;
	}

	const packageDir = getPackageDir();
	cleanupWindowsSelfUpdateQuarantine(packageDir);
	quarantineWindowsNativeDependencies(packageDir);
}

export interface PackageCommandRuntimeOptions {
	extensionFactories?: InlineExtension[];
}

interface CommandSettingsResult {
	settingsManager: SettingsManager;
	projectTrustWarnings: string[];
}

function getCommandAppMode(): AppMode {
	return process.stdin.isTTY && process.stdout.isTTY ? "interactive" : "print";
}

function reportProjectTrustWarnings(warnings: readonly string[]): void {
	for (const warning of warnings) {
		console.error(chalk.yellow(`警告：${warning}`));
	}
}

async function createCommandSettingsManager(options: {
	cwd: string;
	agentDir: string;
	projectTrustOverride?: boolean;
	useSavedProjectTrustOnly?: boolean;
	extensionFactories?: InlineExtension[];
}): Promise<CommandSettingsResult> {
	const settingsManager = SettingsManager.create(options.cwd, options.agentDir, { projectTrusted: false });
	const projectTrustWarnings: string[] = [];
	const trustStore = new ProjectTrustStore(options.agentDir);
	if (options.useSavedProjectTrustOnly) {
		const savedProjectTrusted = trustStore.get(options.cwd) === true;
		settingsManager.setProjectTrusted(options.projectTrustOverride ?? savedProjectTrusted);
		return { settingsManager, projectTrustWarnings };
	}

	const appMode = getCommandAppMode();
	const extensionsResult =
		options.projectTrustOverride === undefined && hasTrustRequiringProjectResources(options.cwd)
			? await new DefaultResourceLoader({
					cwd: options.cwd,
					agentDir: options.agentDir,
					settingsManager,
					extensionFactories: options.extensionFactories,
				}).loadProjectTrustExtensions()
			: undefined;
	for (const error of extensionsResult?.errors ?? []) {
		projectTrustWarnings.push(`加载扩展 "${error.path}" 失败：${error.error}`);
	}

	const projectTrusted = await resolveProjectTrusted({
		cwd: options.cwd,
		trustStore,
		trustOverride: options.projectTrustOverride,
		defaultProjectTrust: settingsManager.getDefaultProjectTrust(),
		extensionsResult,
		projectTrustContext: createProjectTrustContext({
			cwd: options.cwd,
			mode: appMode,
			settingsManager,
			hasUI: appMode === "interactive",
		}),
		onExtensionError: (message) => projectTrustWarnings.push(message),
	});
	settingsManager.setProjectTrusted(projectTrusted);
	return { settingsManager, projectTrustWarnings };
}

export async function handleConfigCommand(
	args: string[],
	runtimeOptions: PackageCommandRuntimeOptions = {},
): Promise<boolean> {
	const [command, ...rest] = args;
	if (command !== "config") {
		return false;
	}

	if (rest.includes("-h") || rest.includes("--help")) {
		printConfigCommandHelp();
		return true;
	}

	let local = false;
	let projectTrustOverride: boolean | undefined;
	for (const arg of rest) {
		if (arg === "-l" || arg === "--local") {
			local = true;
		} else if (arg === "-a" || arg === "--approve") {
			projectTrustOverride = true;
		} else if (arg === "-na" || arg === "--no-approve") {
			projectTrustOverride = false;
		} else if (arg.startsWith("-")) {
			console.error(chalk.red(`"config" 存在未知选项 ${arg}。`));
			console.error(chalk.dim(`使用 "${APP_NAME} --help" 或 "${CONFIG_COMMAND_USAGE}"。`));
			process.exitCode = 1;
			return true;
		} else {
			console.error(chalk.red(`意外的参数 ${arg}。`));
			console.error(chalk.dim(`用法：${CONFIG_COMMAND_USAGE}`));
			process.exitCode = 1;
			return true;
		}
	}

	const cwd = process.cwd();
	const agentDir = getAgentDir();
	const { settingsManager, projectTrustWarnings } = await createCommandSettingsManager({
		cwd,
		agentDir,
		projectTrustOverride,
		extensionFactories: runtimeOptions.extensionFactories,
	});
	reportProjectTrustWarnings(projectTrustWarnings);
	if (local && !settingsManager.isProjectTrusted()) {
		console.error(chalk.red("项目未被信任。请使用 --approve 修改本地资源配置。"));
		process.exitCode = 1;
		return true;
	}
	reportSettingsErrors(settingsManager, "config 命令");
	const globalSettingsManager = SettingsManager.create(cwd, agentDir, { projectTrusted: false });
	const globalResolvedPaths = await new DefaultPackageManager({
		cwd,
		agentDir,
		settingsManager: globalSettingsManager,
	}).resolve();
	const projectResolvedPaths = settingsManager.isProjectTrusted()
		? await new DefaultPackageManager({ cwd, agentDir, settingsManager }).resolve()
		: globalResolvedPaths;

	await selectConfig({
		resolvedPaths: { global: globalResolvedPaths, project: projectResolvedPaths },
		settingsManager,
		cwd,
		agentDir,
		writeScope: local ? "project" : "global",
		projectModeAvailable: settingsManager.isProjectTrusted(),
	});

	process.exit(0);
}

export async function handlePackageCommand(
	args: string[],
	runtimeOptions: PackageCommandRuntimeOptions = {},
): Promise<boolean> {
	const options = parsePackageCommand(args);
	if (!options) {
		return false;
	}

	if (options.help) {
		printPackageCommandHelp(options.command);
		return true;
	}

	if (options.invalidOption) {
		console.error(chalk.red(`"${options.command}" 存在未知选项 ${options.invalidOption}。`));
		console.error(chalk.dim(`使用 "${APP_NAME} --help" 或 "${getPackageCommandUsage(options.command)}"。`));
		process.exitCode = 1;
		return true;
	}

	if (options.missingOptionValue) {
		console.error(chalk.red(`${options.missingOptionValue} 缺少取值。`));
		console.error(chalk.dim(`用法：${getPackageCommandUsage(options.command)}`));
		process.exitCode = 1;
		return true;
	}

	if (options.invalidArgument) {
		console.error(chalk.red(`意外的参数 ${options.invalidArgument}。`));
		console.error(chalk.dim(`用法：${getPackageCommandUsage(options.command)}`));
		process.exitCode = 1;
		return true;
	}

	if (options.conflictingOptions) {
		console.error(chalk.red(options.conflictingOptions));
		console.error(chalk.dim(`用法：${getPackageCommandUsage(options.command)}`));
		process.exitCode = 1;
		return true;
	}

	const source = options.source;
	if ((options.command === "install" || options.command === "remove") && !source) {
		console.error(chalk.red(options.command === "install" ? "缺少安装源。" : "缺少要移除的包源。"));
		console.error(chalk.dim(`用法：${getPackageCommandUsage(options.command)}`));
		process.exitCode = 1;
		return true;
	}

	if (options.command === "update" && options.updateTarget?.type === "models") {
		try {
			await refreshModelCatalogs(getAgentDir());
		} catch (error: unknown) {
			const message = error instanceof Error ? error.message : "模型目录刷新出现未知错误";
			console.error(chalk.red(`错误：${message}`));
			process.exitCode = 1;
		}
		return true;
	}

	const cwd = process.cwd();
	const agentDir = getAgentDir();
	const writesProjectPackageConfig = (options.command === "install" || options.command === "remove") && options.local;
	const { settingsManager, projectTrustWarnings } = await createCommandSettingsManager({
		cwd,
		agentDir,
		projectTrustOverride: options.projectTrustOverride,
		useSavedProjectTrustOnly: options.command === "update",
		extensionFactories: runtimeOptions.extensionFactories,
	});
	reportProjectTrustWarnings(projectTrustWarnings);
	if (!settingsManager.isProjectTrusted() && writesProjectPackageConfig) {
		console.error(chalk.red("项目未被信任。请使用 --approve 修改本地包配置。"));
		process.exitCode = 1;
		return true;
	}
	reportSettingsErrors(settingsManager, "package 命令");
	const selfUpdateNpmCommand = settingsManager.getGlobalSettings().npmCommand;

	const packageManager = new DefaultPackageManager({ cwd, agentDir, settingsManager });

	packageManager.setProgressCallback((event) => {
		if (event.type === "start") {
			process.stdout.write(chalk.dim(`${event.message}\n`));
		}
	});

	try {
		switch (options.command) {
			case "install":
				await packageManager.installAndPersist(source!, { local: options.local });
				console.log(chalk.green(`已安装 ${source}`));
				return true;

			case "remove": {
				const removed = await packageManager.removeAndPersist(source!, { local: options.local });
				if (!removed) {
					console.error(chalk.red(`没有找到匹配 ${source} 的包`));
					process.exitCode = 1;
					return true;
				}
				console.log(chalk.green(`已移除 ${source}`));
				return true;
			}

			case "list": {
				const configuredPackages = packageManager.listConfiguredPackages();
				const userPackages = configuredPackages.filter((pkg) => pkg.scope === "user");
				const projectPackages = configuredPackages.filter((pkg) => pkg.scope === "project");

				if (configuredPackages.length === 0) {
					console.log(chalk.dim("尚未安装任何包。"));
					return true;
				}

				const formatPackage = (pkg: (typeof configuredPackages)[number]) => {
					const display = pkg.filtered ? `${pkg.source}（已过滤）` : pkg.source;
					console.log(`  ${display}`);
					if (pkg.installedPath) {
						console.log(chalk.dim(`    ${pkg.installedPath}`));
					}
				};

				if (userPackages.length > 0) {
					console.log(chalk.bold("用户级包："));
					for (const pkg of userPackages) {
						formatPackage(pkg);
					}
				}

				if (projectPackages.length > 0) {
					if (userPackages.length > 0) console.log();
					console.log(chalk.bold("项目级包："));
					for (const pkg of projectPackages) {
						formatPackage(pkg);
					}
				}

				return true;
			}

			case "update": {
				const target = options.updateTarget ?? { type: "self" };
				if (options.showExtensionsSkippedNote) {
					console.log(chalk.dim(`已跳过扩展。执行 ${APP_NAME} update --extensions 可更新扩展。`));
				}
				if (updateTargetIncludesExtensions(target)) {
					const updateSource = target.type === "extensions" ? target.source : undefined;
					await packageManager.update(updateSource);
					if (updateSource) {
						console.log(chalk.green(`已更新 ${updateSource}`));
					} else {
						console.log(chalk.green("已更新包"));
					}
				}
				if (updateTargetIncludesSelf(target)) {
					const managedInstallRoot = getActiveManagedInstallRoot();
					if (managedInstallRoot && options.force) {
						console.error(chalk.red(`托管安装的 ${APP_NAME} 不支持 --force；请重新运行安装程序修复此安装。`));
						process.exitCode = 1;
						return true;
					}
					const selfUpdatePlan = await getSelfUpdatePlan(options.force);
					if (!selfUpdatePlan.shouldRun) {
						return true;
					}
					if (managedInstallRoot) {
						if (selfUpdatePlan.note) {
							printSelfUpdateNote(selfUpdatePlan.note);
						}
						try {
							console.log(chalk.dim(`正在更新托管安装的 ${APP_NAME}...`));
							await runManagedSelfUpdate(managedInstallRoot, selfUpdatePlan.version);
						} catch (error: unknown) {
							const message = error instanceof Error ? error.message : "托管更新出现未知错误";
							console.error(chalk.red(`错误：${message}`));
							process.exitCode = 1;
							return true;
						}
						console.log(chalk.green(`已将 ${APP_NAME} 从 ${VERSION} 更新到 ${selfUpdatePlan.version}`));
						return true;
					}

					const installMethod = detectInstallMethod();
					if (process.platform === "win32" && installMethod !== "npm" && installMethod !== "pnpm") {
						console.error(chalk.red(`${APP_NAME} 在 Windows 上仅支持 npm 和 pnpm 安装方式的自更新。`));
						console.error(chalk.dim(`检测到的安装方式：${installMethod}。请手动更新 ${APP_NAME}。`));
						process.exitCode = 1;
						return true;
					}
					const selfUpdateTarget = {
						packageName: selfUpdatePlan.packageName,
						installSpec: selfUpdatePlan.installSpec,
					};
					const selfUpdateCommand = getSelfUpdateCommand(PACKAGE_NAME, selfUpdateNpmCommand, selfUpdateTarget);
					if (!selfUpdateCommand) {
						printSelfUpdateUnavailable(selfUpdateNpmCommand, selfUpdateTarget);
						process.exitCode = 1;
						return true;
					}
					if (selfUpdatePlan.note) {
						printSelfUpdateNote(selfUpdatePlan.note);
					}
					try {
						if (installMethod === "npm") {
							prepareWindowsNpmSelfUpdate();
						}
						await runSelfUpdate(selfUpdateCommand);
					} catch (error: unknown) {
						const message = error instanceof Error ? error.message : "包命令出现未知错误";
						console.error(chalk.red(`错误：${message}`));
						if (installMethod === "pnpm") {
							printPnpmSelfUpdateMetadataHint();
						}
						printSelfUpdateFallback(selfUpdateCommand);
						process.exitCode = 1;
						return true;
					}
					console.log(chalk.green(`已将 ${APP_NAME} 从 ${VERSION} 更新到 ${selfUpdatePlan.version}`));
				}
				return true;
			}
		}
	} catch (error: unknown) {
		const message = error instanceof Error ? error.message : "包命令出现未知错误";
		console.error(chalk.red(`错误：${message}`));
		process.exitCode = 1;
		return true;
	}
}
