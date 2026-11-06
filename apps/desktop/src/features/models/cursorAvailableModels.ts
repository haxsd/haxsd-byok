import { configuredPluginModels, type Model, type PluginDescriptor } from "../../shared/api";
import { modelProviderName } from "../../shared/utils/modelProvider";

export function cursorAvailableModels(models: Model[], plugins: PluginDescriptor[]) {
  return [
    ...models.map(model => ({
      id: model.model_hash,
      name: model.display_name,
      source: modelProviderName(model),
    })),
    ...configuredPluginModels(plugins).map(model => ({
      id: model.id,
      name: model.displayName,
      source: model.pluginName,
    })),
  ];
}
