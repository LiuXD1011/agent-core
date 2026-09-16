import { describe, expect, it } from "vitest";
import { buildBaseOptions } from "../src/api/simple-options.ts";
import { generateImages } from "../src/images.ts";
import { registerImagesApiProvider } from "../src/images-api-registry.ts";
import { createImagesModels, createImagesProvider } from "../src/images-models.ts";
import { createModels, createProvider } from "../src/models.ts";
import type {
	Context,
	DeferredHandle,
	ImagesContext,
	ImagesModel,
	Model,
	ProviderRequestOptions,
} from "../src/types.ts";
import { AssistantMessageEventStream } from "../src/utils/event-stream.ts";

const signal = new AbortController().signal;
const context: Context = { messages: [] };
const imagesContext: ImagesContext = { input: [{ type: "text", text: "circle" }] };

const model: Model<"request-test"> = {
	id: "model",
	name: "Model",
	api: "request-test",
	provider: "request-provider",
	baseUrl: "https://example.test",
	reasoning: false,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 1000,
	maxTokens: 100,
};

const imageModel: ImagesModel<"request-test-images"> = {
	id: "image-model",
	name: "Image Model",
	api: "request-test-images",
	provider: "request-image-provider",
	baseUrl: "https://example.test",
	input: ["text"],
	output: ["image"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
};

function completedStream(requestModel: Model<string>): AssistantMessageEventStream {
	const stream = new AssistantMessageEventStream();
	queueMicrotask(() => {
		stream.push({
			type: "done",
			reason: "stop",
			message: {
				role: "assistant",
				content: [],
				api: requestModel.api,
				provider: requestModel.provider,
				model: requestModel.id,
				usage: {
					input: 0,
					output: 0,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 0,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
				stopReason: "stop",
				timestamp: 0,
			},
		});
	});
	return stream;
}

describe("ProviderRequestOptions.signal", () => {
	it("is inherited by every request option surface and simple-stream conversion", () => {
		const options = { signal } satisfies ProviderRequestOptions;
		expect(options.signal).toBe(signal);
		expect(buildBaseOptions(model, context, { signal }).signal).toBe(signal);
	});

	it("survives provider and Models stream/deferred dispatch", async () => {
		const observed: Array<AbortSignal | undefined> = [];
		const handle: DeferredHandle = {
			provider: model.provider,
			modelId: model.id,
			api: model.api,
			id: "response",
		};
		const provider = createProvider({
			id: model.provider,
			auth: { apiKey: { name: "Test", resolve: async () => ({ auth: {} }) } },
			models: [model],
			api: {
				stream: (requestModel, _context, options) => {
					observed.push(options?.signal);
					return completedStream(requestModel);
				},
				streamSimple: (requestModel, _context, options) => {
					observed.push(options?.signal);
					return completedStream(requestModel);
				},
				fetchDeferred: (requestModel, _handle, options) => {
					observed.push(options?.signal);
					return completedStream(requestModel);
				},
				cancelDeferred: async (_requestModel, _handle, options) => {
					observed.push(options?.signal);
				},
			},
		});

		await provider.stream(model, context, { signal }).result();
		await provider.streamSimple(model, context, { signal }).result();
		await provider.fetchDeferred!(model, handle, { signal }).result();
		await provider.cancelDeferred!(model, handle, { signal });

		const models = createModels();
		models.setProvider(provider);
		await models.stream(model, context, { signal }).result();
		await models.streamSimple(model, context, { signal }).result();
		await models.fetchDeferred(model, handle, { signal });
		await models.cancelDeferred(model, handle, { signal });

		expect(observed).toHaveLength(8);
		expect(observed.every((value) => value === signal)).toBe(true);
	});

	it("survives direct and ImagesModels image dispatch", async () => {
		const observed: Array<AbortSignal | undefined> = [];
		registerImagesApiProvider({
			api: imageModel.api,
			generateImages: async (requestModel, _context, options) => {
				observed.push(options?.signal);
				return {
					api: requestModel.api,
					provider: requestModel.provider,
					model: requestModel.id,
					output: [],
					stopReason: "stop",
					timestamp: 0,
				};
			},
		});
		await generateImages(imageModel, imagesContext, { signal });

		const models = createImagesModels();
		models.setProvider(
			createImagesProvider({
				id: imageModel.provider,
				auth: { apiKey: { name: "Test", resolve: async () => ({ auth: {} }) } },
				models: [imageModel],
				api: {
					generateImages: async (requestModel, _context, options) => {
						observed.push(options?.signal);
						return {
							api: requestModel.api,
							provider: requestModel.provider,
							model: requestModel.id,
							output: [],
							stopReason: "stop",
							timestamp: 0,
						};
					},
				},
			}),
		);
		await models.generateImages(imageModel, imagesContext, { signal });

		expect(observed).toEqual([signal, signal]);
	});
});
