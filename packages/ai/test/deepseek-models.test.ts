import { describe, expect, it } from "vitest";
import { getModel, getModels, streamSimple } from "../src/compat.ts";
import { getSupportedThinkingLevels } from "../src/models.ts";
import type { AssistantMessage, Context, Model, Usage } from "../src/types.ts";

const emptyUsage: Usage = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

/** A transcript whose tool result carries an image, as `read` produces for image files. */
function imageContext(model: Model<"openai-completions">): Context {
	const assistantMessage: AssistantMessage = {
		role: "assistant",
		content: [{ type: "toolCall", id: "tool-1", name: "read", arguments: { path: "image.png" } }],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: emptyUsage,
		stopReason: "toolUse",
		timestamp: 2,
	};
	return {
		messages: [
			{ role: "user", content: "What is in this image?", timestamp: 1 },
			assistantMessage,
			{
				role: "toolResult",
				toolCallId: "tool-1",
				toolName: "read",
				content: [
					{ type: "text", text: "Read image file [image/png]" },
					{ type: "image", data: "ZmFrZQ==", mimeType: "image/png" },
				],
				isError: false,
				timestamp: 3,
			},
		],
	};
}

/** Capture the outgoing request body by aborting inside `onPayload`, before any HTTP call. */
async function capturePayload(model: Model<"openai-completions">): Promise<string> {
	let payload: unknown;
	await streamSimple(model, imageContext(model), {
		apiKey: "test-deepseek-key",
		onPayload: (params: unknown) => {
			payload = params;
			throw new Error("payload captured");
		},
	}).result();
	return JSON.stringify(payload);
}

describe("DeepSeek models", () => {
	it("catalogs deepseek-flash and deepseek-v4-pro only", () => {
		expect(getModels("deepseek").map((model) => model.id)).toEqual(["deepseek-flash", "deepseek-v4-pro"]);
	});

	it("advertises image input for deepseek-flash", () => {
		expect(getModel("deepseek", "deepseek-flash").input).toEqual(["text", "image"]);
	});

	it("keeps deepseek-v4-pro text-only", () => {
		expect(getModel("deepseek", "deepseek-v4-pro").input).toEqual(["text"]);
	});

	it("gives deepseek-flash the Flash thinking levels", () => {
		expect(getSupportedThinkingLevels(getModel("deepseek", "deepseek-flash"))).toEqual(["off", "low", "high", "max"]);
	});

	it("sends tool-result images to deepseek-flash as data URLs", async () => {
		const payload = await capturePayload(getModel("deepseek", "deepseek-flash"));

		expect(payload).toContain("data:image/png;base64,ZmFrZQ==");
		expect(payload).not.toContain("model does not support images");
	});

	it("omits tool-result images for the text-only deepseek-v4-pro", async () => {
		const payload = await capturePayload(getModel("deepseek", "deepseek-v4-pro"));

		expect(payload).toContain("tool image omitted: model does not support images");
		expect(payload).not.toContain("data:image/png");
	});
});
