/**
 * 定时器接口。
 *
 * 轮询类逻辑（设备码授权、等 CA 装好）都要能在测试里把时间握在手里：真实 setTimeout 会让
 * 用例变成"睡一会儿再看"，而这里注入替身后可以精确地"触发下一拍"。
 */
export type PollTimers = {
  setTimeout: (handler: () => void, timeoutMs: number) => number;
  clearTimeout: (handle: number) => void;
};

export const browserTimers: PollTimers = {
  setTimeout: (handler, timeoutMs) => window.setTimeout(handler, timeoutMs),
  clearTimeout: (handle) => window.clearTimeout(handle),
};
