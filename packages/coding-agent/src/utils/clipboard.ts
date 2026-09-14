import { type ExecFileSyncOptionsWithStringEncoding, execFileSync } from "child_process";
import { platform } from "os";
import { isWaylandSession } from "./clipboard-image.ts";
import { clipboard } from "./clipboard-native.ts";

type ClipboardReadResult = { ok: true; text: string | null } | { ok: false };

const READ_CLIPBOARD_OPTIONS: ExecFileSyncOptionsWithStringEncoding = {
	encoding: "utf8",
	maxBuffer: 50 * 1024 * 1024,
	timeout: 5000,
};

function readWaylandClipboardText(): ClipboardReadResult {
	try {
		const text = execFileSync("wl-paste", ["--no-newline", "--type", "text"], READ_CLIPBOARD_OPTIONS);
		return { ok: true, text: text || null };
	} catch {
		return { ok: false };
	}
}

/** Read plain text from the system clipboard. */
export async function readClipboardText(): Promise<string | null> {
	if (platform() === "linux" && isWaylandSession() && process.env.WAYLAND_DISPLAY) {
		const result = readWaylandClipboardText();
		if (result.ok) {
			return result.text;
		}
	}

	if (!clipboard) {
		return null;
	}

	try {
		const text = await clipboard.getText();
		return text || null;
	} catch {
		return null;
	}
}
