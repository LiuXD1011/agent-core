import { type Model, modelsAreEqual } from "@liuxuedeng/agent-core-ai";
import {
	Container,
	type Focusable,
	fuzzyFilter,
	getKeybindings,
	Input,
	Spacer,
	Text,
	type TUI,
} from "@liuxuedeng/agent-core-tui";
import type { ModelRuntime } from "../../../app/model-runtime.ts";
import { refreshModelCatalogs } from "../model-catalog-refresh.ts";
import { getModelSelectorSearchText } from "../model-search.ts";
import { theme } from "../theme/theme.ts";
import { DynamicBorder } from "./dynamic-border.ts";
import { keyDisplayText } from "./keybinding-hints.ts";

interface ModelItem {
	provider: string;
	id: string;
	model: Model<any>;
}

interface DefaultModelReference {
	provider: string;
	id: string;
}

/**
 * Component that renders a model selector with search
 */
export class ModelSelectorComponent extends Container implements Focusable {
	private searchInput: Input;

	// Focusable implementation - propagate to searchInput for IME cursor positioning
	private _focused = false;
	get focused(): boolean {
		return this._focused;
	}
	set focused(value: boolean) {
		this._focused = value;
		this.searchInput.focused = value;
	}
	private listContainer: Container;
	private allModels: ModelItem[] = [];
	private filteredModels: ModelItem[] = [];
	private selectedIndex: number = 0;
	private currentModel?: Model<any>;
	private modelRuntime: ModelRuntime;
	private onSelectCallback: (model: Model<any>) => void;
	private onSelectAsDefaultCallback?: (model: Model<any>) => void;
	private onCancelCallback: () => void;
	private errorMessage?: string;
	private refreshStatusMessage = "正在刷新模型目录…";
	private refreshStatusSuccess = false;
	private tui: TUI;
	private defaultModel?: DefaultModelReference;
	private readonly refreshAbortController = new AbortController();
	private refreshTimeout?: ReturnType<typeof setTimeout>;
	private closed = false;

	constructor(
		tui: TUI,
		currentModel: Model<any> | undefined,
		modelRuntime: ModelRuntime,
		onSelect: (model: Model<any>) => void,
		onCancel: () => void,
		initialSearchInput?: string,
		onSelectAsDefault?: (model: Model<any>) => void,
		defaultModel?: DefaultModelReference,
	) {
		super();

		this.tui = tui;
		this.currentModel = currentModel;
		this.modelRuntime = modelRuntime;
		this.defaultModel = defaultModel;
		this.onSelectCallback = onSelect;
		this.onSelectAsDefaultCallback = onSelectAsDefault;
		this.onCancelCallback = onCancel;

		// Add top border
		this.addChild(new DynamicBorder());
		this.addChild(new Spacer(1));

		const hintText = "仅显示已配置服务商的模型。使用 /login 添加服务商。";
		this.addChild(new Text(theme.fg("warning", hintText), 0, 0));
		this.addChild(new Spacer(1));

		// Create search input
		this.searchInput = new Input();
		if (initialSearchInput) {
			this.searchInput.setValue(initialSearchInput);
		}
		this.searchInput.onSubmit = () => {
			// Enter on search input selects the first filtered item
			if (this.filteredModels[this.selectedIndex]) {
				this.handleSelect(this.filteredModels[this.selectedIndex].model);
			}
		};
		this.addChild(this.searchInput);

		this.addChild(new Spacer(1));

		// Create list container
		this.listContainer = new Container();
		this.addChild(this.listContainer);

		this.addChild(new Spacer(1));

		// Hint
		if (this.onSelectAsDefaultCallback) {
			this.addChild(
				new Text(
					theme.fg(
						"dim",
						`  ${keyDisplayText("tui.select.confirm")} 选择 · ${keyDisplayText("app.models.save")} 设为默认 · ${keyDisplayText("tui.select.cancel")} 取消`,
					),
					0,
					0,
				),
			);
		}

		// Add bottom border
		this.addChild(new DynamicBorder());

		// Render the current snapshot immediately, then refresh in the background.
		this.loadModelsFromSnapshot();
		if (initialSearchInput) this.filterModels(initialSearchInput);
		else this.updateList();
		this.tui.requestRender();
		void this.refreshModels();
	}

	private loadModelsFromSnapshot(): void {
		const models = this.modelRuntime.getAvailableSnapshot().map((model: Model<any>) => ({
			provider: model.provider,
			id: model.id,
			model,
		}));
		this.allModels = this.sortModels(models);
		this.filteredModels = this.allModels;
		const currentIndex = this.filteredModels.findIndex((item) => modelsAreEqual(this.currentModel, item.model));
		this.selectedIndex =
			currentIndex >= 0 ? currentIndex : Math.min(this.selectedIndex, Math.max(0, this.filteredModels.length - 1));
	}

	private async refreshModels(): Promise<void> {
		const timeoutMs = 15_000;
		let timedOut = false;
		this.refreshTimeout = setTimeout(() => {
			timedOut = true;
			this.refreshAbortController.abort();
		}, timeoutMs);
		try {
			const result = await refreshModelCatalogs(this.modelRuntime, this.refreshAbortController.signal);
			if (this.closed) return;
			this.refreshStatusMessage = "";
			if (result.aborted && timedOut) {
				this.errorMessage = "模型刷新超时；显示已缓存的模型。";
			} else if (result.errors.size === 1) {
				this.errorMessage = `无法刷新 ${result.errors.keys().next().value}；显示已缓存的模型。`;
			} else if (result.errors.size > 1) {
				this.errorMessage = `无法刷新 ${result.errors.size} 个模型目录（${[...result.errors.keys()].join("、")}）；显示已缓存的模型。`;
			} else {
				this.errorMessage = this.modelRuntime.getError();
				if (!this.errorMessage) {
					this.refreshStatusMessage = "模型目录已刷新。";
					this.refreshStatusSuccess = true;
				}
			}
			this.loadModelsFromSnapshot();
			this.filterModels(this.searchInput.getValue());
			this.tui.requestRender();
		} catch (error) {
			if (this.closed) return;
			this.refreshStatusMessage = "";
			this.errorMessage = timedOut
				? "模型刷新超时；显示已缓存的模型。"
				: `无法刷新模型目录：${error instanceof Error ? error.message : String(error)}`;
			this.updateList();
			this.tui.requestRender();
		} finally {
			if (this.refreshTimeout) clearTimeout(this.refreshTimeout);
		}
	}

	dispose(): void {
		if (this.closed) return;
		this.closed = true;
		if (this.refreshTimeout) clearTimeout(this.refreshTimeout);
		this.refreshAbortController.abort();
	}

	private sortModels(models: ModelItem[]): ModelItem[] {
		const sorted = [...models];
		// Sort: current model first, default model second, then by provider.
		sorted.sort((a, b) => {
			const aIsCurrent = modelsAreEqual(this.currentModel, a.model);
			const bIsCurrent = modelsAreEqual(this.currentModel, b.model);
			if (aIsCurrent && !bIsCurrent) return -1;
			if (!aIsCurrent && bIsCurrent) return 1;
			const aIsDefault = this.isDefaultModel(a.model);
			const bIsDefault = this.isDefaultModel(b.model);
			if (aIsDefault && !bIsDefault) return -1;
			if (!aIsDefault && bIsDefault) return 1;
			return a.provider.localeCompare(b.provider);
		});
		return sorted;
	}

	private isDefaultModel(model: Model<any>): boolean {
		return this.defaultModel?.provider === model.provider && this.defaultModel.id === model.id;
	}

	private isDefaultSearch(query: string): boolean {
		const normalized = query.trim().toLowerCase();
		return normalized.length > 0 && "default".startsWith(normalized);
	}

	private filterModels(query: string): void {
		if (query) {
			const filtered = fuzzyFilter(this.allModels, query, (item) => {
				const defaultText = this.isDefaultModel(item.model) ? " default" : "";
				return `${getModelSelectorSearchText({ id: item.id, provider: item.provider, name: item.model.name })}${defaultText}`;
			});
			if (this.isDefaultSearch(query)) {
				const defaultItems = this.allModels.filter((item) => this.isDefaultModel(item.model));
				const defaultKeys = new Set(defaultItems.map((item) => `${item.provider}\0${item.id}`));
				this.filteredModels = [
					...defaultItems,
					...filtered.filter((item) => !defaultKeys.has(`${item.provider}\0${item.id}`)),
				];
			} else {
				this.filteredModels = filtered;
			}
		} else {
			this.filteredModels = this.allModels;
		}
		// When filtering by a query, move the selector to the top row so the best
		// match is highlighted. When the query is cleared, keep the current position
		// clamped to the (restored) list length.
		this.selectedIndex = query ? 0 : Math.min(this.selectedIndex, Math.max(0, this.filteredModels.length - 1));
		this.updateList();
	}

	private updateList(): void {
		this.listContainer.clear();

		const maxVisible = 10;
		const startIndex = Math.max(
			0,
			Math.min(this.selectedIndex - Math.floor(maxVisible / 2), this.filteredModels.length - maxVisible),
		);
		const endIndex = Math.min(startIndex + maxVisible, this.filteredModels.length);

		// Show visible slice of filtered models
		for (let i = startIndex; i < endIndex; i++) {
			const item = this.filteredModels[i];
			if (!item) continue;

			const isSelected = i === this.selectedIndex;
			const isCurrent = modelsAreEqual(this.currentModel, item.model);
			const isDefault = this.isDefaultModel(item.model);
			const defaultBadge = isDefault ? theme.fg("muted", " · default") : "";

			const cursor = isSelected ? theme.fg("accent", "→ ") : "  ";
			const currentMarker = isCurrent ? theme.fg("accent", "✓ ") : "  ";
			const modelText = isSelected ? theme.fg("accent", item.id) : item.id;
			const providerBadge = theme.fg("muted", `[${item.provider}]`);
			const line = `${cursor}${currentMarker}${modelText} ${providerBadge}${defaultBadge}`;

			this.listContainer.addChild(new Text(line, 0, 0));
		}

		// Add scroll indicator if needed
		if (startIndex > 0 || endIndex < this.filteredModels.length) {
			const scrollInfo = theme.fg("muted", `  (${this.selectedIndex + 1}/${this.filteredModels.length})`);
			this.listContainer.addChild(new Text(scrollInfo, 0, 0));
		}

		// Show error message or "no results" if empty
		if (this.errorMessage) {
			// Show error in red
			const errorLines = this.errorMessage.split("\n");
			for (const line of errorLines) {
				this.listContainer.addChild(new Text(theme.fg("error", line), 0, 0));
			}
		} else if (this.filteredModels.length === 0) {
			this.listContainer.addChild(new Text(theme.fg("muted", "  没有匹配的模型"), 0, 0));
		} else {
			const selected = this.filteredModels[this.selectedIndex];
			this.listContainer.addChild(new Spacer(1));
			this.listContainer.addChild(new Text(theme.fg("muted", `  Model Name: ${selected.model.name}`), 0, 0));
		}
		if (this.refreshStatusMessage) {
			this.listContainer.addChild(new Spacer(1));
			this.listContainer.addChild(
				new Text(theme.fg(this.refreshStatusSuccess ? "success" : "muted", `  ${this.refreshStatusMessage}`), 0, 0),
			);
		}
	}

	handleInput(keyData: string): void {
		const kb = getKeybindings();
		// Up arrow - wrap to bottom when at top
		if (kb.matches(keyData, "tui.select.up")) {
			if (this.filteredModels.length === 0) return;
			this.selectedIndex = this.selectedIndex === 0 ? this.filteredModels.length - 1 : this.selectedIndex - 1;
			this.updateList();
		}
		// Down arrow - wrap to top when at bottom
		else if (kb.matches(keyData, "tui.select.down")) {
			if (this.filteredModels.length === 0) return;
			this.selectedIndex = this.selectedIndex === this.filteredModels.length - 1 ? 0 : this.selectedIndex + 1;
			this.updateList();
		}
		// Enter
		else if (kb.matches(keyData, "tui.select.confirm")) {
			const selectedModel = this.filteredModels[this.selectedIndex];
			if (selectedModel) {
				this.handleSelect(selectedModel.model);
			}
		}
		// Escape or Ctrl+C
		else if (kb.matches(keyData, "tui.select.cancel")) {
			this.dispose();
			this.onCancelCallback();
		}
		// Select and save as default
		else if (kb.matches(keyData, "app.models.save") && this.onSelectAsDefaultCallback) {
			const selectedModel = this.filteredModels[this.selectedIndex];
			if (selectedModel) {
				this.dispose();
				this.onSelectAsDefaultCallback(selectedModel.model);
			}
		}
		// Pass everything else to search input
		else {
			this.searchInput.handleInput(keyData);
			this.filterModels(this.searchInput.getValue());
		}
	}

	private handleSelect(model: Model<any>): void {
		this.dispose();
		this.onSelectCallback(model);
	}

	getSearchInput(): Input {
		return this.searchInput;
	}
}
