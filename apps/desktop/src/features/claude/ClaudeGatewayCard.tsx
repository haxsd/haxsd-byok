import type { ClaudeSettings, ClaudeStatus, Model } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { Icon } from "../../shared/ui/Icon";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { StatusPill } from "../../shared/ui/StatusPill";
import { Switch } from "../../shared/ui/Switch";
import { TitledCard } from "../../shared/ui/TitledCard";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { copyIcon, keyIcon, providerIcon, terminalIcon } from "../../shared/ui/icons";
import { clientErrorText, writeFailureText } from "../harness/harnessErrorText";
import { claudeAliasOptions, claudeEnvText } from "./claudeTakeoverPlan";
import styles from "./ClaudeSettingsPage.module.scss";

/**
 * 两个 tab 共用的「网关」卡。
 *
 * 网关是唯一一份：Claude Code CLI 与 Claude Desktop 只是它的两个消费者。把启用、
 * 端口、默认模型、地址与令牌放在这里，两个 tab 不各写一份，也就不会出现"这个 tab
 * 说开着、那个 tab 说关着"。
 */
export function ClaudeGatewayCard({
  saved,
  draft,
  status,
  models,
  loadError,
  writeError,
  busy,
  onChange,
  onRetry,
  onCopy,
  onRotate,
  rotating,
}: {
  /** 服务端已保存的设置：状态结论以它为准。 */
  saved: ClaudeSettings | null;
  draft: ClaudeSettings | null;
  status: ClaudeStatus | null;
  models: Model[];
  loadError: unknown;
  writeError: unknown;
  busy: boolean;
  onChange: (settings: ClaudeSettings) => void;
  onRetry: () => void;
  onCopy: (text: string, done: string) => void;
  onRotate: () => void;
  rotating: boolean;
}) {
  const listening = status?.listening ?? false;
  const dirty = Boolean(draft && saved && JSON.stringify(draft) !== JSON.stringify(saved));
  const canEnable = Boolean(draft?.default_model_hash);
  const selectedHashes = draft
    ? [draft.default_model_hash, draft.sonnet_model_hash, draft.opus_model_hash, draft.haiku_model_hash]
        .filter((hash): hash is string => Boolean(hash))
    : [];
  const modelOptions = [
    { value: "", label: t("未选择"), group: t("默认") },
    ...claudeAliasOptions(models, selectedHashes).filter((option) => option.value !== ""),
  ];

  return <TitledCard
    title={t("本机网关")}
    description={t("Claude Code 与 Claude Desktop 共用这个网关")}
    icon={providerIcon}
    badge={status
      ? <StatusPill tone={listening ? "ok" : status.enabled ? "warn" : "idle"}>
        {listening ? t("已监听") : status.enabled ? t("未监听") : t("未启用")}
      </StatusPill>
      : undefined}
  >
    <div className={styles.gateway}>
      {loadError != null && <div className={styles.errorRow} role="alert">
        <span>{t("读取 Claude 设置失败：{error}", { error: clientErrorText(loadError) })}</span>
        <Button size="small" disabled={busy} onClick={onRetry}>{t("重试")}</Button>
      </div>}
      {writeError != null && <div className={styles.errorRow} role="alert">
        <span>{writeFailureText(writeError)}</span>
      </div>}

      <div className={styles.settingRow}>
        <div>
          <strong>{t("启用 Claude 网关")}</strong>
          <small>{busy
            ? t("保存中…")
            : !canEnable
              ? t("先选择默认模型")
              : dirty
                ? t("有未保存的改动，点右上角「保存」")
                : t("关闭时不监听端口")}</small>
        </div>
        <Switch
          label={t("启用 Claude 网关")}
          checked={draft?.enabled ?? false}
          disabled={!draft || busy || (!draft.enabled && !canEnable)}
          onChange={(enabled) => { if (draft) onChange({ ...draft, enabled }); }}
        />
      </div>

      <div className={styles.fields}>
        <FormField label={t("默认模型")} hint={t("客户端发来未知模型时回落到它")}>
          <ModelSelect
            mode="single"
            label={t("选择 Claude 默认模型")}
            options={modelOptions}
            value={draft?.default_model_hash ?? ""}
            disabled={!draft || busy}
            onChange={(hash) => { if (draft) onChange({ ...draft, default_model_hash: hash || null }); }}
          />
        </FormField>
        <FormField label={t("网关端口")} hint={t("保存后立即按新端口监听")}>
          <TextInput
            aria-label={t("网关端口")}
            type="number"
            min={1}
            max={65535}
            value={draft?.port ?? 0}
            disabled={!draft || busy}
            onChange={(event) => { if (draft) onChange({ ...draft, port: Number(event.target.value) }); }}
          />
        </FormField>
      </div>

      <div className={styles.facts}>
        <div className={styles.fact}>
          <Icon icon={terminalIcon} size="1.1em" />
          <div>
            <strong>{t("网关地址")}</strong>
            <span className={styles.copyValue}>
              <code>{status?.base_url || t("读取中…")}</code>
              <TooltipTrigger label={t("复制网关地址")}>
                <button
                  type="button"
                  aria-label={t("复制网关地址")}
                  disabled={!status?.base_url}
                  onClick={() => onCopy(status?.base_url ?? "", t("网关地址已复制"))}
                ><Icon icon={copyIcon} size="1em" /></button>
              </TooltipTrigger>
            </span>
          </div>
        </div>
        <div className={styles.fact}>
          <Icon icon={keyIcon} size="1.1em" />
          <div>
            <strong>{t("访问令牌")}</strong>
            <span className={styles.copyValue}>
              <code>{status?.token || t("读取中…")}</code>
              <TooltipTrigger label={t("复制 token")}>
                <button
                  type="button"
                  aria-label={t("复制 token")}
                  disabled={!status?.token}
                  onClick={() => onCopy(status?.token ?? "", t("token 已复制"))}
                ><Icon icon={copyIcon} size="1em" /></button>
              </TooltipTrigger>
              <Button
                size="small"
                disabled={!status?.token || rotating || busy}
                onClick={onRotate}
              >{rotating ? t("轮换中…") : t("重新生成")}</Button>
            </span>
          </div>
        </div>
      </div>

      <div className={styles.gatewayFooter}>
        <span>{t("可发现 {count} 个模型", { count: status?.models ?? 0 })}</span>
        <span>{t("已记录 {count} 次调用", { count: status?.calls ?? 0 })}</span>
        <Button
          size="small"
          disabled={!status?.base_url || !status?.token}
          onClick={() => onCopy(claudeEnvText(status, saved, models), t("环境变量已复制"))}
        >{t("复制环境变量")}</Button>
      </div>

      {status?.last_error && <p className={styles.error} role="alert">
        {t("网关最近的错误：{error}", { error: clientErrorText(status.last_error) })}
      </p>}
    </div>
  </TitledCard>;
}
