import { useEffect, useState } from "react";
import { api, type AppInfo } from "../../shared/api";
import { useAppStore } from "../../shared/store/appStore";
import { Button } from "../../shared/ui/Button";
import { TitledCard } from "../../shared/ui/TitledCard";
import { useMessage } from "../../shared/ui/message";
import styles from "./SettingsPage.module.scss";

/**
 * 诊断卡片：把「发给开发者一份现场」需要的信息和动作收在一处。
 *
 * 日志与诊断现场都在 `<数据目录>\logs`（`server/src/diagnostics.rs` 抓的
 * `diagnostics-*.log` 也在那里），但以前只有「启动失败」的原生弹窗会写出这个路径——
 * 其它错误发生时报错的人拿不到任何可以转发的东西。
 *
 * 只读展示 + 两个动作：打开日志目录、复制诊断摘要。摘要里带上版本、可执行文件路径
 * 与数据目录，因为本机可以同时存在两份安装，"跑的是哪一份"是排查的第一步。
 */
export function DiagnosticsCard() {
  const { ports, devinStatus, offline } = useAppStore();
  const message = useMessage();
  const [appInfo, setAppInfo] = useState<AppInfo | null>(null);
  const [appInfoError, setAppInfoError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    let cancelled = false;
    void api.appInfo()
      .then((value) => {
        if (!cancelled) setAppInfo(value);
      })
      .catch((cause) => {
        if (!cancelled) setAppInfoError(cause instanceof Error ? cause.message : String(cause));
      });
    return () => {
      cancelled = true;
    };
  }, []);

  const logDirectory = appInfo ? joinPath(appInfo.data_dir, "logs") : null;
  const gateway = offline
    ? t("服务未连接")
    : devinStatus?.listening ? t("运行中") : devinStatus?.enabled ? t("待重启") : t("已关闭");
  // 端口 0 表示启动时随机分配；这里读的是设置里保存的值，不是当前实际绑定的端口。
  const portsText = ports.proxy_port === 0 || ports.service_port === 0
    ? t("代理 {proxy} / 服务 {service}（0 表示启动时随机选择）", { proxy: ports.proxy_port, service: ports.service_port })
    : t("代理 {proxy} / 服务 {service}", { proxy: ports.proxy_port, service: ports.service_port });
  const loading = t("读取中…");
  const failed = t("读取失败");

  const summary = () => [
    `haxsd byok ${appInfo?.version ?? failed}`,
    `${t("可执行文件")}: ${appInfo?.executable_path ?? failed}`,
    `${t("数据目录")}: ${appInfo?.data_dir ?? failed}`,
    `${t("日志目录")}: ${logDirectory ?? failed}`,
    `${t("监听端口")}: ${portsText}`,
    `${t("本机网关")}: ${gateway}`,
    `${t("诊断现场")}: ${logDirectory ? joinPath(logDirectory, "diagnostics-<时间戳>.log") : failed}`,
  ].join("\n");

  const openLogs = async () => {
    try {
      setBusy(true);
      await api.openLogDirectory();
    } catch (cause) {
      message.error(cause);
    } finally {
      setBusy(false);
    }
  };

  const copySummary = async () => {
    try {
      setBusy(true);
      await api.copyCursorText(summary());
      message(t("诊断摘要已复制"));
    } catch (cause) {
      message.error(cause);
    } finally {
      setBusy(false);
    }
  };

  return <TitledCard title={t("诊断")} description={t("出问题时，把这里的信息和日志发给开发者")}>
    <div className={styles.runtimeRow}>
      <strong>{t("应用版本")}</strong>
      <small>{appInfo?.version ?? (appInfoError ? failed : loading)}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("可执行文件")}</strong>
      <small title={appInfoError ?? undefined}>{appInfo?.executable_path ?? (appInfoError ? failed : loading)}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("数据目录")}</strong>
      <small title={appInfoError ?? undefined}>{appInfo?.data_dir ?? (appInfoError ? failed : loading)}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("日志目录")}</strong>
      <small title={appInfoError ?? undefined}>{logDirectory ?? (appInfoError ? failed : loading)}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("监听端口")}</strong>
      <small>{portsText}</small>
    </div>
    <div className={styles.runtimeRow}>
      <strong>{t("本机网关")}</strong>
      <small>{gateway}</small>
    </div>
    <div className={styles.diagnosticsActions}>
      <Button size="small" disabled={busy} onClick={() => void openLogs()}>{t("打开日志目录")}</Button>
      <Button size="small" disabled={busy} onClick={() => void copySummary()}>{t("复制诊断摘要")}</Button>
    </div>
  </TitledCard>;
}

/** 数据目录与子目录的拼接：Windows 的数据目录是 `\`，其它平台是 `/`。 */
function joinPath(base: string, child: string) {
  const separator = base.includes("\\") ? "\\" : "/";
  return `${base.replace(/[\\/]+$/, "")}${separator}${child}`;
}
