import { APP_NAME } from "../config.ts";
import type { SourceInfo } from "./source-info.ts";

export type SlashCommandSource = "extension" | "prompt" | "skill";

export interface SlashCommandInfo {
	name: string;
	description?: string;
	source: SlashCommandSource;
	sourceInfo: SourceInfo;
}

export interface BuiltinSlashCommand {
	name: string;
	description: string;
	argumentHint?: string;
}

export const BUILTIN_SLASH_COMMANDS: ReadonlyArray<BuiltinSlashCommand> = [
	{ name: "settings", description: "打开设置菜单" },
	{ name: "model", description: "选择模型（打开选择器）", argumentHint: "<provider/model>" },
	{ name: "tree", description: "浏览会话树（切换分支）" },
	{ name: "thinking", description: "设置推理强度", argumentHint: "<级别>" },
	{ name: "export", description: "导出会话（默认 HTML，也可指定 .html/.jsonl 路径）" },
	{ name: "import", description: "从 JSONL 文件导入并恢复会话" },
	{ name: "name", description: "设置会话显示名称" },
	{ name: "session", description: "显示会话信息与统计" },
	{ name: "changelog", description: "显示变更记录" },
	{ name: "hotkeys", description: "显示全部快捷键" },
	{ name: "fork", description: "从历史用户消息创建分支" },
	{ name: "clone", description: "在当前位置复制当前会话" },
	{ name: "trust", description: "保存本项目信任决定，供后续会话使用" },
	{ name: "login", description: "配置服务商认证", argumentHint: "<服务商>" },
	{ name: "logout", description: "移除服务商认证" },
	{ name: "new", description: "开始新会话" },
	{ name: "compact", description: "手动压缩会话上下文" },
	{ name: "resume", description: "切换到其他会话" },
	{ name: "reload", description: "重新加载快捷键、扩展、技能、提示模板、主题和上下文文件" },
	{ name: "quit", description: `退出 ${APP_NAME}` },
];
