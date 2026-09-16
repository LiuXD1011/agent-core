export function getPiUserAgent(version: string): string {
	const runtime = process.versions.bun ? `bun/${process.versions.bun}` : `node/${process.version}`;
	return `agent-core/${version} (${process.platform}; ${runtime}; ${process.arch})`;
}
