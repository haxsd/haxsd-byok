import type { ClaudeDesktopTakeover, ClaudeSettings, ClaudeStatus, Model, ClaudeTakeoverConflict } from "../../shared/api";
import { Button } from "../../shared/ui/Button";
import { FormField } from "../../shared/ui/FormControls";
import { Icon } from "../../shared/ui/Icon";
import { ModelSelect } from "../../shared/ui/ModelSelect";
import { StatusPill } from "../../shared/ui/StatusPill";
import { TooltipTrigger } from "../../shared/ui/TooltipTrigger";
import { copyIcon } from "../../shared/ui/icons";
import { writeFailureText } from "../harness/harnessErrorText";
import { ClaudeConflictList } from "./ClaudeConflictList";
import {
  claudeConfigSourceLabel,
  claudeDesktopGuided,
  claudeTakeoverDescription,
  claudeTakeoverStateLabel,
  claudeTakeoverStateTone,
  type ClaudeActionGate,
} from "./claudeConnectionView";
import {
  claudeDesktopGuideItems,
  claudeModelLabel,
  claudeModelOptions,
  moveClaudeModel,
} from "./claudeTakeoverPlan";
import styles from "./ClaudeSettingsPage.module.scss";

/**
 * Claude Desktop 的接管 tab。
 *
 * 两条路径互斥：能写配置时给模型排序并接管；受管或只能手工配置时改走引导视图，
 * 只展示要填的值和复制按钮，不摆一个实际不写文件的"应用"按钮。
 */
export function ClaudeDesktopTab({
  draft,
  status,
  takeover,
  models,
  desktopModels,
  gate,
  busy,
  error,
  keptAfterUndo,
  onDesktopModelsChange,
  onApply,
  onUndo,
  onCopy,
}: {
  draft: ClaudeSettings | null;
  status: ClaudeStatus | null;
  takeover: ClaudeDesktopTakeover | null;
  models: Model[];
  desktopModels: string[];
  gate: ClaudeActionGate;
  busy: boolean;
  error: unknown;
  /** 最近一次撤销里服务端保留的条目；没有撤销过或没有保留时为空。 */
  keptAfterUndo: ClaudeTakeoverConflict[];
  onDesktopModelsChange: (hashes: string[]) => void;
  onApply: () => void;
  onUndo: () => void;
  onCopy: (text: string, done: string) => void;
}) {
  const guided = claudeDesktopGuided(takeover);
  const running = takeover?.state === "running";
  const options = claudeModelOptions(models, desktopModels);
  const guideItems = claudeDesktopGuideItems({
    baseUrl: status?.base_url ?? "",
    token: status?.token ?? "",
    models: desktopModels,
    modelNames: (hash) => claudeModelLabel(models, hash),
  });

  return <div className={styles.tab}>
    <div className={styles.tabHeader}>
      <div>
        <strong>Claude Desktop</strong>
        <small>{claudeTakeoverDescription("desktop", takeover?.state ?? null)}</small>
      </div>
      {takeover && <StatusPill tone={claudeTakeoverStateTone(takeover.state)}>{claudeTakeoverStateLabel(takeover.state)}</StatusPill>}
    </div>

    <dl className={styles.factList}>
      <div>
        <dt>{t("安装状态")}</dt>
        <dd>{takeover
          ? takeover.state === "missing" ? t("没有检测到 Claude Desktop") : t("已安装")
          : t("读取中…")}</dd>
      </div>
      <div>
        <dt>{t("运行状态")}</dt>
        <dd>{takeover
          ? running ? t("正在运行") : t("未运行")
          : t("读取中…")}</dd>
      </div>
      <div>
        <dt>{t("配置来源")}</dt>
        <dd>{takeover ? claudeConfigSourceLabel(takeover.config_source) : t("读取中…")}</dd>
      </div>
      <div>
        <dt>{t("profile 路径")}</dt>
        <dd>{takeover?.profile_path ? <code>{takeover.profile_path}</code> : t("未检测到")}</dd>
      </div>
      <div>
        <dt>{t("备份")}</dt>
        <dd>{takeover?.backup_path ? <code>{takeover.backup_path}</code> : t("还没有接管备份")}</dd>
      </div>
    </dl>

    <ClaudeConflictList conflicts={takeover?.conflicts ?? []} stale={takeover?.state === "stale"} />

    {keptAfterUndo.length > 0 && <section className={styles.kept} aria-label={t("撤销时保留的改动")}>
      <strong>{t("撤销保留了这些改动")}</strong>
      <ul>
        {keptAfterUndo.map((conflict, index) => <li key={`${conflict.code}-${conflict.key ?? index}`}>
          {conflict.message}
        </li>)}
      </ul>
      <small>{t("这些内容在接管之后被改过，撤销没有覆盖它们。")}</small>
    </section>}

    {running && <section className={styles.blocker} role="alert">
      <strong>{t("先完全退出 Claude Desktop")}</strong>
      <small>{t("它正在运行，配置文件可能被占用。请从托盘或任务管理器完全退出（不是只关窗口），再回到这里应用。")}</small>
    </section>}

    {guided
      ? <section className={styles.section} aria-label={t("手工配置引导")}>
        <div className={styles.sectionHeading}>
          <strong>{t("在 Claude Desktop 里手工配置")}</strong>
          <small>{t("打开 Developer → Configure Third-Party Inference，按下面的值逐项填写。这条路径由客户端自己管理，本应用不写 profile。")}</small>
        </div>
        <ol className={styles.guideList}>
          {guideItems.map((item, index) => <li key={item.key} className={styles.guideItem}>
            <span className={styles.guideIndex}>{index + 1}</span>
            <div className={styles.guideBody}>
              <strong>{item.label}</strong>
              <span className={styles.guideValue}><code>{item.value || t("读取中…")}</code>
                <TooltipTrigger label={t("复制{label}", { label: item.label })}>
                  <button
                    type="button"
                    aria-label={t("复制{label}", { label: item.label })}
                    disabled={!item.value}
                    onClick={() => onCopy(item.value, t("{label} 已复制", { label: item.label }))}
                  ><Icon icon={copyIcon} size="1em" /></button>
                </TooltipTrigger>
              </span>
              <small>{item.hint}</small>
            </div>
          </li>)}
        </ol>
        {gate.undo && <>
          <div className={styles.sectionHeading}>
            <strong>{t("撤销会移除本应用写入的内容")}</strong>
            <small>{t("撤销移除本应用写入的 profile 与 configLibrary 条目，并还原原来的 appliedId；你之后改过 profile 时会保留并报告，不会静默删除。")}</small>
          </div>
          <div className={styles.actions}>
            <Button disabled={busy} onClick={onUndo}>{busy ? t("撤销中…") : t("撤销接管")}</Button>
          </div>
        </>}
      </section>
      : <section className={styles.section} aria-label={t("Desktop 模型列表")}>
        <div className={styles.sectionHeading}>
          <strong>{t("模型列表")}</strong>
          <small>{t("第一项是默认模型；顺序会写进 Claude Desktop 的模型列表。")}</small>
        </div>
        <FormField label={t("添加模型")} hint={t("来自共享模型库")}>
          <ModelSelect
            mode="single"
            label={t("选择要添加的 Claude Desktop 模型")}
            options={[
              { value: "", label: t("选择要添加的模型"), group: t("模型库") },
              ...options.filter((option) => !desktopModels.includes(option.value)),
            ]}
            value=""
            disabled={busy}
            onChange={(hash) => {
              if (hash && !desktopModels.includes(hash)) onDesktopModelsChange([...desktopModels, hash]);
            }}
          />
        </FormField>
        {desktopModels.length === 0
          ? <p className={styles.note}>{t("还没有选择模型。至少选一个，第一项会作为默认模型。")}</p>
          : <ol className={styles.modelList}>
            {desktopModels.map((hash, index) => <li key={hash} className={styles.modelRow}>
              <span className={styles.modelIndex}>{index + 1}</span>
              <div className={styles.modelName}>
                <strong>{claudeModelLabel(models, hash)}</strong>
                <code>{hash}</code>
              </div>
              {index === 0 && <StatusPill tone="ok">{t("默认")}</StatusPill>}
              <div className={styles.modelActions}>
                <button
                  type="button"
                  aria-label={t("上移 {model}", { model: claudeModelLabel(models, hash) })}
                  disabled={busy || index === 0}
                  onClick={() => onDesktopModelsChange(moveClaudeModel(desktopModels, index, -1))}
                >↑</button>
                <button
                  type="button"
                  aria-label={t("下移 {model}", { model: claudeModelLabel(models, hash) })}
                  disabled={busy || index === desktopModels.length - 1}
                  onClick={() => onDesktopModelsChange(moveClaudeModel(desktopModels, index, 1))}
                >↓</button>
                <button
                  type="button"
                  aria-label={t("移除 {model}", { model: claudeModelLabel(models, hash) })}
                  disabled={busy}
                  onClick={() => onDesktopModelsChange(desktopModels.filter((candidate) => candidate !== hash))}
                >{t("移除")}</button>
              </div>
            </li>)}
          </ol>}

        <section className={styles.section} aria-label={t("撤销接管")}>
          <div className={styles.sectionHeading}>
            <strong>{t("撤销会移除本应用写入的内容")}</strong>
            <small>{t("撤销移除本应用写入的 profile 与 configLibrary 条目，并还原原来的 appliedId；你之后改过 profile 时会保留并报告，不会静默删除。")}</small>
          </div>
        </section>

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
        </div>
      </section>}

    {error != null && <p className={styles.error} role="alert">{writeFailureText(error)}</p>}

    {!guided && !draft && <p className={styles.note}>{t("读取中…")}</p>}
  </div>;
}
