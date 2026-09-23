import type { IconifyIcon } from "@iconify/react/offline";
import { useEffect, useRef, useState, type ReactNode } from "react";
import Sortable from "sortablejs";
import type { Model, PluginModelDescriptor } from "../../shared/api";
import { Card } from "../../shared/ui/Card";
import { Icon } from "../../shared/ui/Icon";
import { ActionMenu, type ActionMenuItem } from "../../shared/ui/ActionMenu";
import { chevronRightIcon, claudeIcon, dragIcon, dotsIcon, flatColorOrganizationIcon, openAiIcon, playIcon } from "../../shared/ui/icons";
import { CursorModelTestResult, type CursorModelTestState } from "./CursorModelTestResult";
import styles from "./CursorSettings.module.scss";

export type CursorModelGrouping = "flat" | "provider" | "type";

export type CursorModelGroup = {
  key: string;
  label: string;
  icon: IconifyIcon;
  models: Model[];
};

type CursorModelCardsProps = {
  models: Model[];
  pluginModels: PluginModelDescriptor[];
  grouping: CursorModelGrouping;
  disabled: boolean;
  /** 搜索筛选生效时关闭拖拽：排序提交的是完整列表，筛选后的子集提交会破坏顺序。 */
  allowReorder: boolean;
  testingModelHashes: Set<string>;
  testResults: Map<string, CursorModelTestState>;
  onTest: (model: Model) => void;
  onEdit: (model: Model) => void;
  onDuplicate: (model: Model) => void;
  onDelete: (model: Model) => void;
  onTestPluginModel: (model: PluginModelDescriptor) => void;
  onPluginSettings: (model: PluginModelDescriptor) => void;
  onReorder: (modelHashes: string[]) => void;
  onGroupSettings: (group: CursorModelGroup) => void;
};

type ModelGridProps = Omit<CursorModelCardsProps, "grouping" | "pluginModels" | "onTestPluginModel" | "onPluginSettings"> & {
  sortable: boolean;
};
export function cursorModelGroups(models: Model[], grouping: Exclude<CursorModelGrouping, "flat">): CursorModelGroup[] {
  const groups = new Map<string, CursorModelGroup>();
  for (const model of models) {
    const descriptor = grouping === "provider" ? providerGroup(model) : typeGroup(model);
    const group = groups.get(descriptor.key);
    if (group) {
      group.models.push(model);
    } else {
      groups.set(descriptor.key, { ...descriptor, models: [model] });
    }
  }
  return [...groups.values()];
}

export function CursorModelCards(props: CursorModelCardsProps) {
  const builtins = props.grouping === "flat"
    ? <ModelGrid {...props} sortable={props.allowReorder} />
    : <div className={styles.modelGroups}>
      {cursorModelGroups(props.models, props.grouping).map((group) => <CollapsibleGroup
        key={group.key}
        label={group.label}
        icon={group.icon}
        count={group.models.length}
        defaultOpen={false}
        onSettings={props.grouping === "provider" ? () => props.onGroupSettings(group) : undefined}
      >
        {group.models.map((model) => <ModelListRow
          key={model.model_hash}
          model={model}
          disabled={props.disabled}
          testing={props.testingModelHashes.has(model.model_hash)}
          result={props.testResults.get(model.model_hash)}
          onTest={() => props.onTest(model)}
          onEdit={() => props.onEdit(model)}
          onDuplicate={() => props.onDuplicate(model)}
          onDelete={() => props.onDelete(model)}
        />)}
      </CollapsibleGroup>)}
    </div>;
  return <div className={styles.modelGroups}>
    {builtins}
    {pluginGroups(props.pluginModels).map((group) => <CollapsibleGroup
      key={`${props.grouping}:${group.pluginId}`}
      label={group.pluginName}
      iconSrc={group.icon}
      count={group.models.length}
      defaultOpen={props.grouping === "flat"}
    >
      {group.models.map((model) => <PluginModelRow
        key={model.id}
        model={model}
        disabled={props.disabled}
        testing={props.testingModelHashes.has(model.id)}
        result={props.testResults.get(model.id)}
        onTest={() => props.onTestPluginModel(model)}
        onSettings={() => props.onPluginSettings(model)}
      />)}
    </CollapsibleGroup>)}
  </div>;
}

function pluginGroups(models: PluginModelDescriptor[]) {
  const groups: { pluginId: string; pluginName: string; icon: string; models: PluginModelDescriptor[] }[] = [];
  for (const model of models) {
    let group = groups.find((candidate) => candidate.pluginId === model.pluginId);
    if (!group) {
      group = { pluginId: model.pluginId, pluginName: model.pluginName, icon: model.icon, models: [] };
      groups.push(group);
    }
    group.models.push(model);
  }
  return groups;
}

function CollapsibleGroup({ label, icon, iconSrc, count, defaultOpen = true, onSettings, children }: {
  label: string;
  icon?: IconifyIcon;
  iconSrc?: string;
  count: number;
  defaultOpen?: boolean;
  onSettings?: () => void;
  children: ReactNode;
}) {
  const [open, setOpen] = useState(defaultOpen);
  return <Card className={styles.groupCard}>
    <div className={styles.groupHeader}>
      <button
        type="button"
        className={styles.groupToggle}
        aria-expanded={open}
        onClick={() => setOpen((current) => !current)}
      >
        <span className={styles.groupChevron} data-open={open}><Icon icon={chevronRightIcon} size="1em" /></span>
        {icon && <Icon icon={icon} size="1.1em" />}
        {iconSrc && <Icon src={iconSrc} size="1.1em" />}
        <span className={styles.groupLabel}>{label}</span>
        <span className={styles.groupCount}>{count}</span>
      </button>
      {onSettings && <button type="button" className={styles.groupSettings} onClick={onSettings}>
        {t("分组设置")}
      </button>}
    </div>
    {open && <div className={styles.modelList}>{children}</div>}
  </Card>;
}

function ModelListRow({ model, disabled, testing, result, onTest, onEdit, onDuplicate, onDelete }: {
  model: Model;
  disabled: boolean;
  testing: boolean;
  result: CursorModelTestState | undefined;
  onTest: () => void;
  onEdit: () => void;
  onDuplicate: () => void;
  onDelete: () => void;
}) {
  return <div className={styles.modelRow}>
    <div className={styles.modelRowName}>
      <span className={styles.modelRowNameText}>{model.display_name}</span>
      <span className={styles.modelRowModelId}>{model.model_id}</span>
    </div>
    <div className={styles.modelRowChips}>
      <ModelChips model={model} />
    </div>
    <CursorModelTestResult compact state={result} testing={testing} />
    <div className={styles.modelCardActions}>
      <button type="button" className={styles.testButton} disabled={disabled && !testing} onClick={onTest}>
        <Icon icon={playIcon} size="1em" />{testing ? t("取消测试") : t("测试")}
      </button>
      <ActionMenu label={t("更多")} disabled={disabled} items={rowActions({ onEdit, onDuplicate, onDelete })} />
    </div>
  </div>;
}

function PluginModelRow({ model, disabled, testing, result, onTest, onSettings }: {
  model: PluginModelDescriptor;
  disabled: boolean;
  testing: boolean;
  result: CursorModelTestState | undefined;
  onTest: () => void;
  onSettings: () => void;
}) {
  return <div className={styles.modelRow}>
    <div className={styles.modelRowName}>
      <span className={styles.modelRowNameText}>{model.displayName}</span>
      <span className={styles.modelRowModelId}>{model.modelId}</span>
    </div>
    <div className={styles.modelRowChips}>
      <span className={styles.chip}>{t("插件提供")}</span>
      {model.images && <span className={styles.chip}>{t("支持图片")}</span>}
    </div>
    <CursorModelTestResult compact state={result} testing={testing} />
    <div className={styles.modelCardActions}>
      <button type="button" className={styles.testButton} disabled={disabled && !testing} onClick={onTest}>
        <Icon icon={playIcon} size="1em" />{testing ? t("取消测试") : t("测试")}
      </button>
      <button type="button" className={styles.secondaryAction} disabled={disabled} onClick={onSettings}>{t("设置")}</button>
    </div>
  </div>;
}

function rowActions({ onEdit, onDuplicate, onDelete }: {
  onEdit: () => void;
  onDuplicate: () => void;
  onDelete: () => void;
}): ActionMenuItem[] {
  return [
    { id: "edit", label: t("编辑"), onSelect: onEdit },
    { id: "duplicate", label: t("复制"), onSelect: onDuplicate },
    { id: "delete", label: t("删除"), onSelect: onDelete },
  ];
}

/**
 * 一张模型卡要说清三件事：它是什么（名称、模型 ID、协议），它有多大（上下文、输出上限），
 * 它现在还通不通（最近一次测试）。原来只有前两行的名字和 ID，其余靠点开编辑器去看。
 */
function ModelGrid({
  models,
  sortable: sortableEnabled,
  disabled,
  testingModelHashes,
  testResults,
  onTest,
  onEdit,
  onDuplicate,
  onDelete,
  onReorder,
}: ModelGridProps) {
  const grid = useRef<HTMLDivElement>(null);
  const sortable = useRef<Sortable | null>(null);
  const currentModels = useRef(models);
  const reorder = useRef(onReorder);
  currentModels.current = models;
  reorder.current = onReorder;

  useEffect(() => {
    if (!sortableEnabled || !grid.current) return;
    sortable.current = Sortable.create(grid.current, {
      animation: 160,
      dataIdAttr: "data-model-hash",
      draggable: `.${styles.modelCard}`,
      handle: `.${styles.sortHandle}`,
      ghostClass: styles.sortGhost,
      chosenClass: styles.sortChosen,
      dragClass: styles.sortDragging,
      forceFallback: true,
      fallbackOnBody: true,
      fallbackTolerance: 3,
      onEnd: (event) => {
        const oldIndex = event.oldDraggableIndex ?? event.oldIndex;
        const newIndex = event.newDraggableIndex ?? event.newIndex;
        if (typeof oldIndex !== "number"
          || typeof newIndex !== "number"
          || oldIndex === newIndex) {
          sortable.current?.sort(currentModels.current.map((model) => model.model_hash), false);
          return;
        }
        const reordered = currentModels.current.slice();
        const [moved] = reordered.splice(oldIndex, 1);
        if (!moved || newIndex < 0 || newIndex > reordered.length) {
          sortable.current?.sort(currentModels.current.map((model) => model.model_hash), false);
          return;
        }
        reordered.splice(newIndex, 0, moved);
        reorder.current(reordered.map((model) => model.model_hash));
      },
    });
    return () => {
      sortable.current?.destroy();
      sortable.current = null;
    };
  }, [sortableEnabled]);

  useEffect(() => {
    sortable.current?.option("disabled", disabled);
    sortable.current?.sort(models.map((model) => model.model_hash), false);
  }, [disabled, models]);

  return <div ref={grid} className={styles.modelGrid}>
    {models.map((model) => {
      const result = testResults.get(model.model_hash);
      const testing = testingModelHashes.has(model.model_hash);
      return <Card className={styles.modelCard} data-model-hash={model.model_hash} key={model.model_hash}>
        {sortableEnabled && <button type="button" className={styles.sortHandle} disabled={disabled} aria-label={t("拖动排序")} title={t("拖动排序")} onClick={(event) => event.stopPropagation()}>
          <Icon icon={dragIcon} size="1.25em" />
        </button>}
        <div className={styles.modelCardContent}>
          <div className={styles.modelCardTop}>
            <div className={styles.modelCardName}>
              <span className={styles.modelCardNameText} title={model.display_name}>{model.display_name}</span>
              <span className={styles.modelCardModelId} title={model.model_id}>{model.model_id}</span>
            </div>
            <ActionMenu icon={dotsIcon} quiet label={t("更多")} disabled={disabled} items={rowActions({
              onEdit: () => onEdit(model),
              onDuplicate: () => onDuplicate(model),
              onDelete: () => onDelete(model),
            })} />
          </div>
          <div className={styles.modelCardMeta}>
            <span className={styles.providerBadge}>
              <Icon icon={model.type === "anthropic" ? claudeIcon : openAiIcon} />
              {model.type === "anthropic" ? "Anthropic" : endpointLabel(model)}
            </span>
            <ModelChips model={model} />
          </div>
          <div className={styles.modelCardTest}>
            <CursorModelTestResult state={result} testing={testing} />
          </div>
          <div className={styles.modelCardActions}>
            <button type="button" className={styles.testButton} disabled={disabled && !testing} onClick={() => onTest(model)}>
              <Icon icon={playIcon} size="1em" />{testing ? t("取消测试") : t("测试")}
            </button>
            <button type="button" className={styles.secondaryAction} disabled={disabled} onClick={() => onEdit(model)}>{t("编辑")}</button>
            {model.group_name && <span className={styles.groupTag} title={t("分组名称")}>{model.group_name}</span>}
          </div>
        </div>
      </Card>;
    })}
  </div>;
}

/** 上下文、输出上限和思考强度：决定一个模型能不能接住这个活的三个数字。 */
function ModelChips({ model }: { model: Model }) {
  const maxOutput = model.type === "openai" ? model.max_completion_tokens : model.anthropic_max_tokens;
  return <>
    {model.context_window_tokens !== null && <span className={styles.chip}>
      {t("上下文 {tokens}", { tokens: formatTokens(model.context_window_tokens) })}
    </span>}
    {maxOutput !== null && <span className={styles.chip}>
      {t("输出上限 {tokens}", { tokens: formatTokens(maxOutput) })}
    </span>}
    {model.reasoning_effort && <span className={styles.chip} data-tone="accent">{t("思考 {effort}", { effort: model.reasoning_effort })}</span>}
    {model.anthropic_thinking_effort && <span className={styles.chip} data-tone="accent">{t("思考 {effort}", { effort: model.anthropic_thinking_effort })}</span>}
    {model.custom_headers_enabled && <span className={styles.chip}>{t("自定义 Headers")}</span>}
  </>;
}

function formatTokens(value: number) {
  if (value >= 1_000_000) return `${Number((value / 1_000_000).toFixed(1))}M`;
  if (value >= 1000) return `${Number((value / 1000).toFixed(value % 1000 === 0 ? 0 : 1))}K`;
  return String(value);
}

function endpointLabel(model: Model) {
  if (model.use_full_url) return "OpenAI · URL";
  if (model.openai_endpoint === "/v1/chat/completions") return "OpenAI · Chat";
  return "OpenAI · Responses";
}

function providerGroup(model: Model) {
  const key = providerDomain(model.base_url);
  const label = model.group_name?.trim() || key;
  return { key, label, icon: flatColorOrganizationIcon };
}

function providerDomain(baseUrl: string) {
  const value = baseUrl.trim();
  try {
    return new URL(value).hostname.toLowerCase() || value;
  } catch {
    try {
      return new URL(`https://${value}`).hostname.toLowerCase() || value;
    } catch {
      return value;
    }
  }
}

function typeGroup(model: Model) {
  if (model.type === "anthropic") return { key: "anthropic", label: "Anthropic", icon: claudeIcon };
  if (model.openai_endpoint === "/v1/chat/completions") return { key: "openai-chat", label: "OpenAI Chat", icon: openAiIcon };
  return { key: "openai-responses", label: "OpenAI Responses", icon: openAiIcon };
}
