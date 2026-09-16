const INTEGER_FORMAT = new Intl.NumberFormat("en-US", { maximumFractionDigits: 0 });

export function formatSavingsCount(value: number, unit: string): string {
	const count = Number.isFinite(value) ? Math.max(0, Math.round(value)) : 0;
	return `${INTEGER_FORMAT.format(count)} ${unit}`;
}

function compactDecimal(value: number): string {
	return value.toFixed(1).replace(/\.0$/u, "");
}

export function formatSavingsBytes(value: number): string {
	const bytes = Number.isFinite(value) ? Math.max(0, Math.round(value)) : 0;
	if (bytes >= 1024 * 1024) {
		return `${compactDecimal(bytes / (1024 * 1024))} MiB removed from future prompts`;
	}
	if (bytes >= 1024) return `${compactDecimal(bytes / 1024)} KiB removed from future prompts`;
	return `${INTEGER_FORMAT.format(bytes)} B removed from future prompts`;
}
