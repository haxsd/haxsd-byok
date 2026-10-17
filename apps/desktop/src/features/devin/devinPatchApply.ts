import { api, type DevinHostPatchStatus } from "../../shared/api";

/** 「应用补丁」的三种结局。写失败与「写成功但状态读不回来」必须分开说。 */
export type HostPatchOutcome =
  | { kind: "applied"; status: DevinHostPatchStatus }
  | { kind: "write-failed"; cause: unknown }
  | { kind: "status-unreadable" };

export type HostPatchDeps = {
  apply?: (path: string) => Promise<unknown>;
  readStatus?: (path: string) => Promise<DevinHostPatchStatus>;
};

/**
 * 应用 Devin 宿主补丁。
 *
 * 两段式，边界在「文件有没有被改」：
 * - 写入失败 → `write-failed`：文件保持原样，"应用补丁"仍然可以再点。
 * - 写入成功、状态读不回来 → `status-unreadable`：**不能报"应用失败"**——补丁已经落盘，
 *   说失败会诱导用户再点一次，而第二次会被服务端以"已经是打过补丁的版本"拒绝。
 *   界面据此清掉旧状态、指向「检查宿主」。
 * - 都成功 → `applied`：带着状态回去，成功提示只在此时发。
 */
export async function applyDevinHostPatch(hostPath: string, deps: HostPatchDeps = {}): Promise<HostPatchOutcome> {
  const apply = deps.apply ?? ((path: string) => api.applyDevinHostPatch(path));
  const readStatus = deps.readStatus ?? ((path: string) => api.devinHostStatus(path));

  try {
    await apply(hostPath);
  } catch (cause) {
    return { kind: "write-failed", cause };
  }
  try {
    return { kind: "applied", status: await readStatus(hostPath) };
  } catch {
    return { kind: "status-unreadable" };
  }
}
