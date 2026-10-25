import { api, type DevinHostStatus } from "../../shared/api";

/** 「写入设置」的三种结局。写失败与「写成功但状态读不回来」必须分开说。 */
export type HostConfigurationOutcome =
  | { kind: "applied"; status: DevinHostStatus }
  | { kind: "write-failed"; cause: unknown }
  | { kind: "status-unreadable" };

export type HostConfigurationDeps = {
  apply?: (path: string, installRoot: string) => Promise<unknown>;
  readStatus?: (path: string, installRoot: string) => Promise<DevinHostStatus>;
};

export function canRestoreDevinHost(status: DevinHostStatus | null): boolean {
  return Boolean(status?.restore_receipt || status?.extension.restore_receipt);
}

/**
 * 写入 Devin 的用户设置。两段式，边界在「文件有没有被改」：
 * - 写入失败 → `write-failed`：文件保持原样，按钮仍然可以再点。
 * - 写入成功、状态读不回来 → `status-unreadable`：设置已经落盘，不能报成写入失败，
 *   否则用户会重复写一次。界面据此清掉旧状态并指向「检查设置」。
 * - 都成功 → `applied`：带着状态回去，成功提示只在此时发。
 */
export async function applyDevinHostConfiguration(hostPath: string, installRoot = "", deps: HostConfigurationDeps = {}): Promise<HostConfigurationOutcome> {
  const apply = deps.apply ?? ((path: string, root: string) => api.applyDevinHostConfiguration(path, root));
  const readStatus = deps.readStatus ?? ((path: string, root: string) => api.devinHostStatus(path, root));

  try {
    await apply(hostPath, installRoot);
  } catch (cause) {
    return { kind: "write-failed", cause };
  }
  try {
    return { kind: "applied", status: await readStatus(hostPath, installRoot) };
  } catch {
    return { kind: "status-unreadable" };
  }
}
