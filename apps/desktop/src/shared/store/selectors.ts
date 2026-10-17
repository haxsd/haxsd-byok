import type { AppSnapshot } from "../store/appStore";
import type { LlmCall, Model } from "../api";

/**
 * 外壳与命令面板真正关心的字段。
 *
 * 单独放一层是因为这些组件的重渲染成本高（命令面板要重建整个命令表、外壳要重排导航）：
 * 用 `useAppStoreSelector` 时，**选择器返回的必须是快照里的稳定引用**——这里直接取字段，
 * 不组装新对象。写测试盯住这一点：一次不相关的写入之后，选出来的值必须是同一个引用，
 * 否则 `Object.is` 判定为"变了"，组件照样重渲染。
 */

/** 服务是否连不上（离线横幅唯一关心的字段）。 */
export function selectOffline(snapshot: AppSnapshot): boolean {
  return snapshot.offline;
}

/** 顶栏与命令面板要的数据：调用列表、模型列表、当前主题。 */
export function selectCalls(snapshot: AppSnapshot): LlmCall[] {
  return snapshot.calls;
}

export function selectModels(snapshot: AppSnapshot): Model[] {
  return snapshot.models;
}

export function selectTheme(snapshot: AppSnapshot): AppSnapshot["theme"] {
  return snapshot.theme;
}
