// 失败响应的说法：正文裁剪（P3-1）与写请求超时（P3-2）。
//
// 以前不管什么正文都直接塞进提示条：反代挡在前面时正文是一整页 HTML，界面上就是一坨标签；
// 而写请求共用读请求的 15 秒超时，超时被报成"失败"，可服务端可能已经落库了。

import assert from "node:assert/strict";
import test from "node:test";
import { readFile } from "node:fs/promises";
import { installBrowserDoubles, startModuleLoader } from "./harness.mjs";

installBrowserDoubles();
const loader = await startModuleLoader();
test.after(() => loader.close());

const bodies = () => loader.load("/src/shared/utils/errorBody.ts");

test("a JSON error keeps the server sentence and loses nothing important", async () => {
  const { errorBodyMessage } = await bodies();

  assert.equal(
    errorBodyMessage(400, "Bad Request", JSON.stringify({ code: "invalid_argument", message: "端口不能为 0" })),
    "端口不能为 0",
  );
});

test("a plain-text error is shown, but clipped to one readable line", async () => {
  const { errorBodyMessage, ERROR_BODY_LIMIT } = await bodies();

  assert.equal(errorBodyMessage(502, "Bad Gateway", "upstream refused the connection"), "upstream refused the connection");
  const long = "x".repeat(ERROR_BODY_LIMIT * 3);
  const clipped = errorBodyMessage(500, "Internal Server Error", long);
  assert.equal(clipped.length, ERROR_BODY_LIMIT + 1, "超长正文被截断（末尾一个省略号）");
  assert.ok(clipped.endsWith("…"));
});

test("an HTML error page never becomes the toast body", async () => {
  const { errorBodyMessage, errorBodyNeedsConsole } = await bodies();

  const html = "<!doctype html><html><body><h1>502 Bad Gateway</h1><p>nginx</p></body></html>";
  const message = errorBodyMessage(502, "Bad Gateway", html);
  assert.equal(message, "502 Bad Gateway", "HTML 只留状态码，标签不进提示条");
  assert.doesNotMatch(message, /[<>]/);
  assert.equal(errorBodyNeedsConsole(message, html), true, "被丢掉的正文要在控制台留现场");
});

test("an empty body still produces a status line", async () => {
  const { errorBodyMessage } = await bodies();
  assert.equal(errorBodyMessage(500, "Internal Server Error", "   "), "500 Internal Server Error");
});

test("writes get a longer timeout than reads", async () => {
  const { requestTimeoutMs } = await loader.load("/src/shared/api.ts");

  const read = requestTimeoutMs("GET");
  const head = requestTimeoutMs(undefined);
  const write = requestTimeoutMs("PUT");

  assert.equal(read, head, "缺省方法按读处理");
  assert.ok(write > read, "写请求要留出落库时间");
  assert.equal(write, 60_000);
  assert.equal(requestTimeoutMs("DELETE"), write, "删除也是写");
  assert.equal(requestTimeoutMs("post"), write, "方法大小写不敏感");
  assert.equal(requestTimeoutMs("HEAD"), read);

  // 光有这个函数不算：请求本身得真的按方法取默认值，否则写请求还是 15 秒。
  const source = await readFile(new URL("../src/shared/api.ts", import.meta.url), "utf8");
  assert.match(source, /requestTimeoutMs\(rest\.method/, "request() 的默认超时必须按方法分档");
});

test("a timed-out write says the result is unknown instead of failed", async () => {
  const { writeFailureText, clientErrorText } = await loader.load("/src/features/harness/harnessErrorText.ts");
  const { RequestTimeoutError } = await loader.load("/src/shared/api.ts");

  const text = writeFailureText(new RequestTimeoutError());
  assert.match(text, /结果/, "写超时要说清结果未知");
  assert.match(text, /刷新确认/);
  assert.notEqual(text, clientErrorText(new RequestTimeoutError()), "不能和读超时共用一句话");

  // 其它失败照旧走服务端原因的翻译。
  assert.equal(writeFailureText(new Error("端口不能为 0")), clientErrorText(new Error("端口不能为 0")));
  assert.match(writeFailureText(new Error("Devin is still running; quit Devin completely before changing its settings")), /还在运行/);
});

// 复核 12-devin/P3-2：服务端把"备份在哪"放在句末的 `backup: <路径>`，翻译成中文时不能
// 把路径整句吃掉——用户读到"备份和收据不一致"却看不到路径，就没法照做下一步。
test("restore failures keep the backup path the server pointed at", async () => {
  const { clientErrorText } = await loader.load("/src/features/harness/harnessErrorText.ts");
  const backup = "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json.devin-router.backup";

  const receiptRefusal = clientErrorText(new Error(
    "configuration error: no usable Devin settings restore receipt for " +
    "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json " +
    `(copy the kept previous file ${backup} over settings.json after closing Devin); ` +
    `the file was left unchanged; backup: ${backup}`,
  ));
  assert.match(receiptRefusal, /没有可用的恢复记录/, "仍然要有中文原因");
  assert.ok(receiptRefusal.includes(backup), "备份路径要留在界面文案里");
  assert.doesNotMatch(receiptRefusal, /configuration error/, "前缀仍是实现细节");

  const changedAfterConfiguration = clientErrorText(new Error(
    `configuration error: Devin settings changed after this application configured them; ` +
    `restore refused; the previous file is kept at ${backup}; restore backup: ${backup}`,
  ));
  assert.ok(changedAfterConfiguration.includes(backup), "换过设置时也要带出备份路径");

  const unreadable = clientErrorText(new Error(
    "configuration error: cannot read Devin settings file " +
    "C:\\Users\\me\\AppData\\Roaming\\Devin\\User\\settings.json: " +
    "The system cannot find the file specified. (os error 2); close Devin and check that the " +
    `file is readable; restore backup: ${backup}`,
  ));
  assert.ok(unreadable.includes(backup), "设置文件读不到时也要带出备份路径");
});

test("custom Devin installation path errors say which path is wrong", async () => {
  const { clientErrorText } = await loader.load("/src/features/harness/harnessErrorText.ts");

  assert.match(clientErrorText(new Error("configuration error: Devin installation path must be absolute")), /安装路径.*绝对路径/);
  assert.match(clientErrorText(new Error("configuration error: D:\\Devin.exe is not a Devin extension.js file")), /不是 Devin 的 extension\.js/);
  assert.match(clientErrorText(new Error("configuration error: D:\\empty is not a Devin installation directory; expected resources\\app")), /不是 Devin 安装目录/);
  assert.match(clientErrorText(new Error("configuration error: Devin installation path not found: D:\\gone")), /安装路径.*仍然存在/);
  assert.match(clientErrorText(new Error("configuration error: Devin installation not found; 2 locations were searched")), /安装目录/);
});
