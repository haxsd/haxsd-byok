import type { ClaudeCliTakeover, ClaudeSettings, ClaudeStatus, Model, ClaudeTakeoverConflict } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField } from "../../shared/ui/FormControls";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { StatusPill } from "../../shared/ui/StatusPill";
import { writeFailureText } from "../harness/harnessErrorText";
import { ClaudeConflictList } from "./ClaudeConflictList";
import {
  claudeTakeoverDescription,
  claudeTakeoverStateLabel,
  claudeTakeoverStateTone,
  type ClaudeActionGate,
} from "./claudeConnectionView";
import {
  claudeAliasOptions,
  claudeCliEnvKeys,
  claudeCliPreview,
  claudeModelLabel,
} from "./claudeTakeoverPlan";
import styles from "./ClaudeSettingsPage.module.scss";

/**
 * Claude Code CLI 的接管 tab。
 *
 * 顺序按用户实际要做的事排列：先看 CLI 与配置文件在哪，再处理冲突，然后选模型、
 * 看将要写入的 env，最后才是应用/撤销。
 */
export function ClaudeCliTab({
  saved,
  draft,
  status,
  takeover,
  models,
  gate,
  busy,
  error,
  keptAfterUndo,
  onChange,
  onApply,
  onUndo,
  onCopy,
  onOpenModels,
}: {
  saved: ClaudeSettings | null;
  draft: ClaudeSettings | null;
  status: ClaudeStatus | null;
  takeover: ClaudeCliTakeover | null;
  models: Model[];
  gate: ClaudeActionGate;
  busy: boolean;
  error: unknown;
  /** 最近一次撤销里服务端保留的条目；没有撤销过或没有保留时为空。 */
  keptAfterUndo: ClaudeTakeoverConflict[];
  onChange: (settings: ClaudeSettings) => void;
  onApply: () => void;
  onUndo: () => void;
  onCopy: (text: string, done: string) => void;
  onOpenModels: () => void;
}) {
  const selectedHashes = draft
    ? [draft.sonnet_model_hash, draft.opus_model_hash, draft.haiku_model_hash]
        .filter((hash): hash is string => Boolean(hash))
    : [];
  const options = claudeAliasOptions(models, selectedHashes);
  const preview = claudeCliPreview(status, draft, models);

  const mapping = (
    key: "sonnet_model_hash" | "opus_model_hash" | "haiku_model_hash",
    label: string,
    ariaLabel: string,
  ) => <FormField label={label}>
    <ModelSelect
      mode="single"
      label={ariaLabel}
      options={options}
      value={draft?.[key] ?? ""}
      disabled={!draft || busy}
      onChange={(hash) => { if (draft) onChange({ ...draft, [key]: hash || null }); }}
    />
  </FormField>;

  return <div className={styles.tab}>
    <div className={styles.tabHeader}>
      <div>
        <strong>Claude Code CLI</strong>
        <small>{claudeTakeoverDescription("cli", takeover?.state ?? null)}</small>
      </div>
      {takeover && <StatusPill tone={claudeTakeoverStateTone(takeover.state)}>{claudeTakeoverStateLabel(takeover.state)}</StatusPill>}
    </div>

    <dl className={styles.factList}>
      <div>
        <dt>{t("CLI 是否可用")}</dt>
        <dd>{takeover
          ? takeover.state === "missing" ? t("没有找到 Claude Code 的配置文件") : t("已检测到")
          : t("读取中…")}</dd>
      </div>
      <div>
        <dt>settings.json</dt>
        <dd>{takeover?.settings_path
          ? <code>{takeover.settings_path}</code>
          : t("未检测到")}</dd>
      </div>
      <div>
        <dt>{t("备份")}</dt>
        <dd>{takeover?.backup_path
          ? <code>{takeover.backup_path}</code>
          : t("还没有接管备份")}</dd>
      </div>
      {takeover && takeover.managed_sources.length > 0 && <div>
        <dt>{t("受管策略")}</dt>
        <dd><ul className={styles.pathList}>
          {takeover.managed_sources.map((source) => <li key={source}><code>{source}</code></li>)}
        </ul></dd>
      </div>}
    </dl>

    <ClaudeConflictList conflicts={takeover?.conflicts ?? []} />

    {takeover && takeover.missing_models.length > 0 && <section className={styles.hint} aria-label={t("模型库里缺少模型")}>
      <strong>{t("模型库里缺少这些模型")}</strong>
      <ul>
        {takeover.missing_models.map((hash) => <li key={hash}><code>{hash}</code></li>)}
      </ul>
      <small>{t("应用接管前，请先在模型库重新选择。")}</small>
    </section>}

    {keptAfterUndo.length > 0 && <section className={styles.kept} aria-label={t("撤销时保留的改动")}>
      <strong>{t("撤销保留了这些改动")}</strong>
      <ul>
        {keptAfterUndo.map((conflict, index) => <li key={`${conflict.code}-${conflict.key ?? index}`}>
          {conflict.message}
        </li>)}
      </ul>
      <small>{t("这些内容在接管之后被改过，撤销没有覆盖它们。")}</small>
    </section>}

    <section className={styles.section} aria-label={t("模型映射")}>
      <div className={styles.sectionHeading}>
        <strong>{t("模型映射")}</strong>
        <small>{t("Claude Code 按模型家族请求；留空时回落到默认模型。")}</small>
      </div>
      <div className={styles.fields}>
        {mapping("sonnet_model_hash", t("Sonnet 模型"), t("选择 Sonnet 映射模型"))}
        {mapping("opus_model_hash", t("Opus 模型"), t("选择 Opus 映射模型"))}
        {mapping("haiku_model_hash", t("Haiku 模型"), t("选择 Haiku 映射模型"))}
      </div>
    </section>

    <section className={styles.section} aria-label={t("将写入的 settings.json")}>
      <div className={styles.sectionHeading}>
        <strong>{t("将写入的 settings.json")}</strong>
        <small>{t("只读预览；应用后这些键会写进上面的配置文件，已有的同名键会被替换。")}</small>
      </div>
      <pre className={styles.preview} tabIndex={0} aria-label={t("settings.json 预览")}>{preview}</pre>
      <div className={styles.previewActions}>
        <Button size="small" onClick={() => onCopy(preview, t("预览已复制"))}>{t("复制预览")}</Button>
        <Button size="small" onClick={() => onCopy(claudeCliEnvKeys.join("\n"), t("键名已复制"))}>{t("复制键名")}</Button>
      </div>
    </section>

    <section className={styles.section} aria-label={t("撤销接管")}>
      <div className={styles.sectionHeading}>
        <strong>{t("撤销会逐键还原")}</strong>
        <small>{t("撤销只还原当前值仍等于接管值的键；你之后改过的值会保留并在撤销后列出。备份文件只供人工查看，不会自动写回。")}</small>
      </div>
      <ul className={styles.keys}>
        {claudeCliEnvKeys.map((key) => <li key={key}><code>{key}</code></li>)}
      </ul>
    </section>

    {error != null && <p className={styles.error} role="alert">{writeFailureText(error)}</p>}

    <div className={styles.actions}>
      <Button
        variant="primary"
        disabled={!gate.apply || busy}
        onClick={onApply}
      >{busy ? t("应用中…") : t("应用接管")}</Button>
      <Button
        disabled={!gate.undo || busy}
        onClick={onUndo}
      >{busy ? t("撤销中…") : t("撤销接管")}</Button>
      {!gate.apply && gate.applyBlockedReason && <small>{gate.applyBlockedReason}</small>}
      {!gate.apply && gate.applyBlockedLink && <Button size="small" onClick={onOpenModels}>{t("去模型库重新选择")}</Button>}
    </div>

    <p className={styles.note}>{t("默认模型：{model}", { model: saved?.default_model_hash ? claudeModelLabel(models, saved.default_model_hash) : t("未选择") })}</p>
  </div>;
}
