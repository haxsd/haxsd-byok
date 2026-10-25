// 打开 Devin 页时怎么对待"上次记住的宿主路径"。
//
// 记住的路径可能指向已经搬走、卸载或换盘的安装：那不是用户的错误，也不该在页面
// 加载时弹一条红字；这时退回服务端自动探测。只有探测也失败才说明原因，而且由页面
// 内联显示，不打断操作。
export interface HostPathProbe<Status> {
  status: Status | null;
  failure: string | null;
  /** 成功的那次探测实际用了哪几个用户输入；用来清掉已经失效的旧值。 */
  matched: { path: boolean; installRoot: boolean };
}

export async function probeHostPath<Status extends { path: string }>(
  remembered: string,
  inspect: (path?: string, installRoot?: string) => Promise<Status>,
  rememberedInstallRoot = "",
): Promise<HostPathProbe<Status>> {
  const rememberedPath = remembered.trim();
  const installRoot = rememberedInstallRoot.trim();
  const attempts: Array<{ path?: string; installRoot?: string }> = [];
  const addAttempt = (path?: string, root?: string) => {
    if (!attempts.some((attempt) => attempt.path === path && attempt.installRoot === root)) {
      attempts.push({ path, installRoot: root });
    }
  };
  if (rememberedPath || installRoot) addAttempt(rememberedPath || undefined, installRoot || undefined);
  if (installRoot) addAttempt(undefined, installRoot);
  if (rememberedPath) addAttempt(rememberedPath, undefined);
  addAttempt();

  let failure: string | null = null;
  for (const attempt of attempts) {
    try {
      return {
        status: await inspect(attempt.path, attempt.installRoot),
        failure: null,
        matched: { path: Boolean(attempt.path), installRoot: Boolean(attempt.installRoot) },
      };
    } catch (cause) {
      // 两个输入各自失效时互不牵连：先保留另一个已记住的值，再整体退回自动探测。
      failure = cause instanceof Error ? cause.message : String(cause);
    }
  }
  return { status: null, failure, matched: { path: false, installRoot: false } };
}
