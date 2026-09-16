/** Copy observer data without retaining references to mutable session state. */
export function snapshotRecord<T>(value: T): T {
	try {
		return structuredClone(value);
	} catch {
		/* Extension details may contain functions. */
	}
	const copies = new WeakMap<object, unknown>();
	const copy = (item: unknown): unknown => {
		if (typeof item === "function" || typeof item === "symbol") return undefined;
		if (item === null || typeof item !== "object") return item;
		if (copies.has(item)) return copies.get(item);
		if (item instanceof Date) return new Date(item);
		if (item instanceof Uint8Array) return item.slice();
		const result: unknown[] | Record<string, unknown> = Array.isArray(item) ? [] : {};
		copies.set(item, result);
		for (const [key, child] of Object.entries(item)) {
			Object.defineProperty(result, key, {
				value: copy(child),
				enumerable: true,
				writable: true,
				configurable: true,
			});
		}
		return result;
	};
	return copy(value) as T;
}
