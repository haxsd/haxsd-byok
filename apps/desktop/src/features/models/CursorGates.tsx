import type { ReactNode } from "react";
import { useAppStore } from "../../shared/store/appStore";
import { EmptyState } from "../../shared/ui/EmptyState";
import controls from "../../shared/ui/Controls.module.scss";
import { shieldIcon } from "../../shared/ui/icons";

/**
 * CA 是接管的前置条件，所以它必须先被说清：缺证书、证书装了但没被信任、或已就绪，
 * 三种状态对应三种不同的下一步。旧版本这里是一段虚线框里的文字，三种状态混在一起，
 * 用户只能靠读完整句来判断自己处在哪一种。
 */
export function CursorCaGate({ busy, waitingForRefresh, onInitialize, onRefresh, children }: {
  busy: boolean;
  waitingForRefresh: boolean;
  onInitialize: () => void;
  onRefresh: () => void;
  children: ReactNode;
}) {
  // Read the harness state directly. A context that the page had to provide made
  // the gate depend on its caller remembering to wrap it, and the caller stopped.
  const { cursorHarness } = useAppStore();
  if (cursorHarness?.ca === "ready") return children;

  const installedLocally = cursorHarness?.ca === "untrusted";
  const invalid = cursorHarness?.ca === "invalid";
  // 授权这件事在每个平台上不是同一回事：Windows 弹的是 UAC 窗口，不会有人来要密码；
  // macOS 才是终端里的 sudo 提示。以前两处都按 macOS 写，Windows 用户照着找密码框
  // 只会卡住。
  const windows = cursorHarness?.platform === "windows";
  return <EmptyState
    icon={shieldIcon}
    tone={invalid ? "bad" : "info"}
    title={installedLocally
      ? t("本地 CA 已生成，但系统还没信任它")
      : invalid
        ? t("本地 CA 已损坏")
        : t("需要先初始化本地 CA")}
    description={installedLocally
      ? windows
        ? t("安装命令已复制。点下面的按钮打开终端，系统会弹出 UAC 授权窗口，选「是」即可；证书装好后本页会自动刷新。")
        : t("安装命令已复制。点下面的按钮打开终端，按提示输入系统密码；证书装好后本页会自动刷新。")
      : invalid
        ? t("证书文件无法使用，重新初始化会生成一份新的。")
        : t("CA 只保存在本机，用于安全解析 Cursor 的 HTTPS 请求。生成过程不会改动系统设置。")}
    steps={[
      { title: t("生成证书"), detail: t("只需一次，证书保存在本机数据目录。") },
      {
        title: t("在系统中信任它"),
        detail: installedLocally
          ? windows
            ? t("终端会执行安装命令，系统会弹出 UAC 授权窗口。")
            : t("终端会提示你输入系统密码。")
          : windows
            ? t("下一步会打开终端执行安装命令，并弹出 UAC 授权窗口。")
            : t("终端会提示你输入系统密码。"),
      },
      { title: t("开启接管"), detail: t("证书就绪后，本页的接管开关才会解锁。") },
    ]}
    actions={<button
      type="button"
      className={controls.primary}
      disabled={busy}
      onClick={waitingForRefresh ? onRefresh : onInitialize}
    >{busy ? t("刷新中…") : waitingForRefresh ? t("正在等待授权，点此立即刷新") : installedLocally ? t("打开终端安装 CA") : invalid ? t("重新初始化 CA") : t("初始化 CA")}</button>}
  />;
}
