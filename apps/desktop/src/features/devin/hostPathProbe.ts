// 打开 Devin 页时怎么对待"上次记住的宿主路径"。
//
// 记住的路径可能指向已经搬走、卸载或换盘的安装：那不是用户的错误，也不该在页面
// 加载时弹一条红字；这时退回服务端自动探测。只有探测也失败才说明原因，而且由页面
// 内联显示，不打断操作。
export interface HostPathProbe<Status> {
  status: Status | null;
  failure: string | null;
}

export async function probeHostPath<Status extends { path: string }>(
  remembered: string,
  inspect: (path?: string) => Promise<Status>,
): Promise<HostPathProbe<Status>> {
  const rememberedPath = remembered.trim();
  if (rememberedPath) {
    try {
      return { status: await inspect(rememberedPath), failure: null };
    } catch {
      // 落回自动探测；下面那次的失败信息才值得展示。
    }
  }
  try {
    return { status: await inspect(), failure: null };
  } catch (cause) {
    return { status: null, failure: cause instanceof Error ? cause.message : String(cause) };
  }
}
