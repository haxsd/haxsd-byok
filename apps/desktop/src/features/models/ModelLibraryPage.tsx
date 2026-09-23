import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useNavigate } from "react-router-dom";
import { api, configuredPluginModels, type Model, type ModelInput } from "../../shared/api";
import { CursorModelCards, cursorModelGroups, type CursorModelGroup, type CursorModelGrouping } from "./CursorModelCards";
import { CursorModelEditor, emptyCursorModelDraft, type CursorModelDraft } from "./CursorModelEditor";
import { CursorModelTestResult, type CursorModelTestState } from "./CursorModelTestResult";
import { LegacyModelImport } from "./LegacyModelImport";
import styles from "./CursorSettings.module.scss";
import { PageContent } from "../../shell/layout/PageContent";
import { PageActions } from "../../shell/PageActions";
import { Card } from "../../shared/ui/Card";
import { ConfirmDialog } from "../../shared/ui/ConfirmDialog";
import { EmptyState } from "../../shared/ui/EmptyState";
import { FormField, SecretTextInput, TextInput } from "../../shared/ui/FormControls";
import controls from "../../shared/ui/Controls.module.scss";
import { Icon } from "../../shared/ui/Icon";
import { Modal } from "../../shared/ui/Modal";
import { PageTitle } from "../../shared/ui/PageTitle";
import { SearchInput } from "../../shared/ui/SearchInput";
import { Segmented } from "../../shared/ui/Segmented";
import { ServiceOfflineState } from "../../shared/ui/ServiceOfflineState";
import { addIcon, playIcon, providerIcon } from "../../shared/ui/icons";
import { useMessage } from "../../shared/ui/message";
import { appStore, useAppStore } from "../../shared/store/appStore";
import toolbar from "../../shared/ui/Toolbar.module.scss";

/**
 * The model library every harness draws from.
 *
 * It deliberately sits outside the Cursor takeover gate: a model is a shared
 * resource, so adding, editing or testing one must not require a trusted CA or an
 * active takeover. Cursor and Devin are parallel modules that meet here and in
 * the call statistics, nowhere else.
 */
export function ModelLibraryPage() {
  const { models, cursorBusy, plugins, offline } = useAppStore();
  const navigate = useNavigate();
  const message = useMessage();
  const [draft, setDraft] = useState<CursorModelDraft | null>(null);
  const [editing, setEditing] = useState<Model | null>(null);
  const [modelOptions, setModelOptions] = useState<string[]>([]);
  const [discovering, setDiscovering] = useState(false);
  const [deleting, setDeleting] = useState<Model | null>(null);
  const [testingModelHashes, setTestingModelHashes] = useState<Set<string>>(() => new Set());
  const [modelTestResults, setModelTestResults] = useState<Map<string, CursorModelTestState>>(() => new Map());
  const [savingAndTesting, setSavingAndTesting] = useState(false);
  const [batchTesting, setBatchTesting] = useState(false);
  const [grouping, setGrouping] = useState<CursorModelGrouping>("flat");
  const [search, setSearch] = useState("");
  const [settingsGroup, setSettingsGroup] = useState<CursorModelGroup | null>(null);
  const [groupNameDraft, setGroupNameDraft] = useState("");
  const [groupBaseUrlDraft, setGroupBaseUrlDraft] = useState("");
  const [groupApiKeyDraft, setGroupApiKeyDraft] = useState("");
  const [groupSettingsBusy, setGroupSettingsBusy] = useState(false);
  const activeModelTests = useRef(new Map<string, { testId: string; controller: AbortController; cancelling: boolean }>());

  const pluginModels = configuredPluginModels(plugins);
  const testTargets = [
    ...models.map((model) => ({ model_hash: model.model_hash, display_name: model.display_name })),
    ...pluginModels.map((model) => ({ model_hash: model.id, display_name: model.displayName })),
  ];
  const providerGroups = cursorModelGroups(models, "provider");
  const typeGroups = cursorModelGroups(models, "type");
  const canGroupByProvider = providerGroups.length > 1;
  const canGroupByType = typeGroups.length > 1;

  /** 搜索只作用于展示；拖拽排序依然按完整列表提交，所以筛选时关掉拖拽。 */
  const query = search.trim().toLowerCase();
  const visibleModels = useMemo(() => query
    ? models.filter((model) => [model.display_name, model.model_id, model.base_url, model.group_name ?? ""]
      .some((field) => field.toLowerCase().includes(query)))
    : models, [models, query]);
  const visiblePluginModels = useMemo(() => query
    ? pluginModels.filter((model) => [model.displayName, model.modelId, model.providerId]
      .some((field) => field.toLowerCase().includes(query)))
    : pluginModels, [pluginModels, query]);

  const testedOk = [...modelTestResults.values()].filter((state) => state.status === "success").length;
  const testedFailed = [...modelTestResults.values()].filter((state) => state.status === "error").length;

  useEffect(() => {
    if ((grouping === "provider" && !canGroupByProvider) || (grouping === "type" && !canGroupByType)) {
      setGrouping("flat");
    }
  }, [canGroupByProvider, canGroupByType, grouping]);

  const openNew = () => {
    const next = emptyCursorModelDraft();
    next.model.sort_order = models.length + 1;
    setEditing(null);
    setModelOptions([]);
    setDraft(next);
  };
  const openEdit = (model: Model) => {
    setEditing(model);
    setModelOptions([model.model_id]);
    setDraft({
      providerId: `builtin/${model.type}`,
      model: modelInput(model),
      openAIExtraParamsText: JSON.stringify(model.openai_extra_params, null, 2),
      customHeadersText: JSON.stringify(model.custom_headers, null, 2),
      anthropicExtraParamsText: JSON.stringify(model.anthropic_extra_params, null, 2),
    });
  };
  const discover = async (): Promise<boolean> => {
    if (!draft) return false;
    setDiscovering(true);
    try {
      const custom_headers = parseHeaders(draft.customHeadersText);
      const result = await api.discoverModels({
        type: draft.model.type,
        base_url: draft.model.base_url.trim(),
        api_key: draft.model.api_key.trim(),
        custom_headers_enabled: draft.model.custom_headers_enabled,
        custom_headers,
      });
      setModelOptions([...new Set(result.models)]);
      return true;
    } catch (cause) {
      message.error(cause);
      return false;
    } finally {
      setDiscovering(false);
    }
  };
  const persist = async (): Promise<Model | null> => {
    if (!draft) return null;
    const input = draftInput(draft);
    if (editing) return appStore.updateCursorModel(editing.model_hash, input);
    return (await appStore.createModels([input]))?.[0] ?? null;
  };
  const save = async () => {
    try {
      if (await persist()) {
        setDraft(null);
        setEditing(null);
      }
    } catch (cause) {
      message.error(cause);
    }
  };
  const cancelModelTest = async (modelHash: string) => {
    const active = activeModelTests.current.get(modelHash);
    if (!active || active.cancelling) return;
    active.cancelling = true;
    active.controller.abort();
    try {
      await api.cancelModelTest(modelHash, active.testId);
    } catch (cause) {
      message(t("取消测试失败：{error}", { error: errorText(cause) }), { duration: 5000 });
    }
  };
  const cancelAllModelTests = async () => {
    await Promise.all([...activeModelTests.current.keys()].map((modelHash) => cancelModelTest(modelHash)));
  };
  const testModel = async (model: { model_hash: string; display_name: string }, notify = true): Promise<"success" | "failure" | "cancelled"> => {
    if (activeModelTests.current.has(model.model_hash)) {
      await cancelModelTest(model.model_hash);
      return "cancelled";
    }
    const active = { testId: crypto.randomUUID(), controller: new AbortController(), cancelling: false };
    activeModelTests.current.set(model.model_hash, active);
    setTestingModelHashes((current) => new Set(current).add(model.model_hash));
    try {
      const result = await api.testModel(model.model_hash, active.testId, active.controller.signal);
      setModelTestResults((current) => new Map(current).set(model.model_hash, { status: "success", result }));
      if (notify) message(t("模型 {model} 连通性测试成功（{duration} ms）", { model: model.display_name, duration: result.duration_ms }));
      return "success";
    } catch (cause) {
      if (active.cancelling || active.controller.signal.aborted) {
        setModelTestResults((current) => new Map(current).set(model.model_hash, { status: "cancelled" }));
        return "cancelled";
      }
      const error = errorText(cause);
      setModelTestResults((current) => new Map(current).set(model.model_hash, { status: "error", error }));
      if (notify) message(t("连通性测试失败：{error}", { error }), { duration: 5000 });
      return "failure";
    } finally {
      if (activeModelTests.current.get(model.model_hash) === active) activeModelTests.current.delete(model.model_hash);
      setTestingModelHashes((current) => {
        const next = new Set(current);
        next.delete(model.model_hash);
        return next;
      });
    }
  };
  const saveAndTest = async () => {
    setSavingAndTesting(true);
    let saved: Model | null = null;
    try {
      saved = await persist();
    } catch (cause) {
      message.error(cause);
    } finally {
      setSavingAndTesting(false);
    }
    if (!saved) return;
    setEditing(saved);
    await testModel(saved);
    await appStore.refresh();
  };
  const testAllModels = async () => {
    if (!testTargets.length || batchTesting) return;
    setBatchTesting(true);
    try {
      const results = await Promise.all(testTargets.map((model) => testModel(model, false)));
      const successful = results.filter((result) => result === "success").length;
      const failed = results.filter((result) => result === "failure").length;
      const cancelled = results.filter((result) => result === "cancelled").length;
      message(cancelled > 0
        ? t("连通性测试已取消：成功 {successful}，失败 {failed}", { successful, failed })
        : failed === 0
          ? t("全部 {count} 个模型连通性测试成功", { count: testTargets.length })
          : t("连通性测试完成：成功 {successful}，失败 {failed}", { successful, failed }),
      { duration: failed === 0 && cancelled === 0 ? 2400 : 5000 });
    } finally {
      setBatchTesting(false);
    }
  };
  const duplicateModel = async (model: Model) => {
    const names = new Set(models.map((item) => item.display_name));
    const baseName = t("{name} 副本", { name: model.display_name });
    let displayName = baseName;
    let suffix = 2;
    while (names.has(displayName)) {
      displayName = `${baseName} ${suffix}`;
      suffix += 1;
    }
    const created = await appStore.createModels([{
      ...modelInput(model),
      sort_order: models.length + 1,
      display_name: displayName,
    }]);
    if (created) message(t("模型已复制"));
  };
  const openGroupSettings = (group: CursorModelGroup) => {
    setGroupNameDraft(group.models.find((model) => model.group_name?.trim())?.group_name?.trim() ?? "");
    setGroupBaseUrlDraft(sharedValue(group.models.map((model) => model.base_url)) ?? "");
    setGroupApiKeyDraft(sharedValue(group.models.map((model) => model.api_key)) ?? "");
    setSettingsGroup(group);
  };
  const saveGroupSettings = async () => {
    if (!settingsGroup) return;
    const group_name = groupNameDraft.trim() || null;
    const base_url = groupBaseUrlDraft.trim();
    const api_key = groupApiKeyDraft.trim();
    setGroupSettingsBusy(true);
    try {
      for (const model of settingsGroup.models) {
        const input: ModelInput = {
          ...modelInput(model),
          group_name,
          ...(base_url ? { base_url } : {}),
          ...(api_key ? { api_key } : {}),
        };
        if (input.group_name === (model.group_name ?? null)
          && input.base_url === model.base_url
          && input.api_key === model.api_key) continue;
        await api.updateModel(model.model_hash, input);
      }
      await appStore.refresh();
      setSettingsGroup(null);
    } catch (cause) {
      message.error(cause);
    } finally {
      setGroupSettingsBusy(false);
    }
  };
  const reorderModels = useCallback(async (modelHashes: string[]) => {
    if (!await appStore.reorderCursorModels(modelHashes)) {
      message(appStore.getSnapshot().error || t("排序失败"));
    }
  }, [message]);

  const list = offline && models.length === 0
    ? <ServiceOfflineState />
    : query && visibleModels.length === 0 && visiblePluginModels.length === 0
    ? <EmptyState
      icon={providerIcon}
      title={t("没有匹配的模型")}
      description={t("换个关键词，或清除搜索框。")}
      actions={<button type="button" className={controls.secondary} onClick={() => setSearch("")}>{t("清除筛选")}</button>}
    />
    : <LegacyModelImport>{({ busy: importingLegacyModels, previewing, open }) =>
      models.length === 0 && pluginModels.length === 0
        ? <EmptyState
          icon={providerIcon}
          title={t("模型库还是空的")}
          description={t("添加一个模型后，Cursor 与 Devin 都能用它；也可以从本机旧版配置里导入。")}
          steps={[
            { title: t("填服务器地址与 API Key"), detail: t("任何 OpenAI 兼容或 Anthropic 兼容的接口都可以。") },
            { title: t("点“发现模型”拉取模型 ID"), detail: t("也可以手动填写模型 ID。") },
            { title: t("保存并测试连通性"), detail: t("测试通过后，模型立刻可以给两个客户端使用。") },
          ]}
          actions={<>
            <button type="button" className={controls.primary} disabled={cursorBusy || importingLegacyModels} onClick={openNew}>{t("添加模型")}</button>
            <button type="button" className={controls.secondary} disabled={cursorBusy || importingLegacyModels} onClick={open}>{previewing ? t("读取中…") : t("导入旧版配置")}</button>
            <button type="button" className={controls.secondary} onClick={() => void navigate("/tutorial")}>{t("先看使用教程")}</button>
          </>}
        />
        : <CursorModelCards
          models={visibleModels}
          pluginModels={visiblePluginModels}
          grouping={grouping}
          disabled={cursorBusy || importingLegacyModels}
          allowReorder={query === ""}
          testingModelHashes={testingModelHashes}
          testResults={modelTestResults}
          onTest={(model) => void testModel(model)}
          onEdit={openEdit}
          onDuplicate={(model) => void duplicateModel(model)}
          onDelete={setDeleting}
          onTestPluginModel={(model) => void testModel({ model_hash: model.id, display_name: model.displayName })}
          onPluginSettings={() => void navigate("/plugins")}
          onReorder={reorderModels}
          onGroupSettings={openGroupSettings}
        />
    }</LegacyModelImport>;

  const estimatedListViewport = grouping === "flat" ? Math.ceil(models.length / 3) * 210 + 260 : 420;

  const content = <div className={styles.page}>
    <Card className={styles.modelToolbar}>
      <SearchInput
        className={styles.modelSearch}
        value={search}
        onValueChange={setSearch}
        ariaLabel={t("搜索模型")}
        placeholder={t("搜模型名称、ID 或地址…")}
      />
      <Segmented
        ariaLabel={t("模型分组方式")}
        value={grouping}
        options={[
          { value: "flat", label: t("默认平铺") },
          ...(canGroupByProvider ? [{ value: "provider" as CursorModelGrouping, label: t("按供应商") }] : []),
          ...(canGroupByType ? [{ value: "type" as CursorModelGrouping, label: t("按类型") }] : []),
        ]}
        onChange={setGrouping}
      />
      <span className={toolbar.spacer} />
      <button
        type="button"
        className={styles.batchTest}
        disabled={cursorBusy || (!batchTesting && testingModelHashes.size > 0)}
        onClick={() => void (batchTesting ? cancelAllModelTests() : testAllModels())}
      >
        <Icon icon={playIcon} size="1.05em" />
        {batchTesting ? t("取消全部测试") : t("一键测试")}
      </button>
    </Card>
    {models.length > 0 && <div className={toolbar.facts}>
      <Fact label={t("模型")} value={String(models.length)} />
      <Fact label={t("供应商")} value={String(providerGroups.length)} />
      <Fact label={t("插件模型")} value={String(pluginModels.length)} />
      <Fact label={t("测试通过")} value={String(testedOk)} tone={testedOk > 0 ? "ok" : "none"} />
      <Fact label={t("测试失败")} value={String(testedFailed)} tone={testedFailed > 0 ? "bad" : "none"} />
    </div>}
    {list}
  </div>;

  return <>
    <PageActions>
      <button type="button" className={styles.addModel} disabled={cursorBusy} onClick={openNew}>
        <Icon icon={addIcon} size="1.05em" />{t("添加模型")}
      </button>
    </PageActions>
    <PageContent
      title={<PageTitle
        title={t("模型库")}
        meta={t("Cursor 与 Devin 共用同一份模型配置")}
      />}
      sections={[{ key: "model-library", estimatedHeight: Math.max(360, estimatedListViewport), content }]}
    />
    <Modal fullHeight open={draft !== null} title={editing ? t("编辑模型") : t("添加模型")} banner={draft && (editorTesting(editing, testingModelHashes) || (editing && modelTestResults.get(editing.model_hash))) ? <CursorModelTestResult state={editing ? modelTestResults.get(editing.model_hash) : undefined} testing={editorTesting(editing, testingModelHashes)} /> : undefined} busy={cursorBusy || savingAndTesting} onClose={() => { if (editing && editorTesting(editing, testingModelHashes)) void cancelModelTest(editing.model_hash); setDraft(null); setEditing(null); }} onSubmit={() => void save()} submitLabel={t("保存")} secondaryAction={<button type="button" className={controls.secondary} disabled={cursorBusy || savingAndTesting} onClick={() => void (editing && editorTesting(editing, testingModelHashes) ? cancelModelTest(editing.model_hash) : saveAndTest())}>{savingAndTesting ? t("处理中…") : editing && editorTesting(editing, testingModelHashes) ? t("取消测试") : t("保存并测试")}</button>}>
      {draft && <CursorModelEditor draft={draft} modelOptions={modelOptions} discovering={discovering} onChange={setDraft} onDiscover={discover} />}
    </Modal>
    <Modal open={settingsGroup !== null} title={t("分组设置")} busy={groupSettingsBusy || cursorBusy} onClose={() => setSettingsGroup(null)} onSubmit={() => void saveGroupSettings()} submitLabel={t("保存")}>
      {settingsGroup && <div className={styles.editor}>
        <FormField label={t("分组名称")} hint={t("应用于该分组下的全部模型，并作为模型选择器中的徽章标签；清空则恢复显示服务器域名。")}>
          <TextInput placeholder={settingsGroup.key} value={groupNameDraft} onChange={(event) => setGroupNameDraft(event.target.value)} />
        </FormField>
        <FormField label={t("服务器地址")} hint={t("修改后应用于该分组下的全部模型；留空保持各模型现有配置不变。")}>
          <TextInput placeholder={t("留空保持不变")} value={groupBaseUrlDraft} onChange={(event) => setGroupBaseUrlDraft(event.target.value)} />
        </FormField>
        <FormField label="API Key" hint={t("修改后应用于该分组下的全部模型；留空保持各模型现有配置不变。")}>
          <SecretTextInput placeholder={t("留空保持不变")} autoComplete="off" value={groupApiKeyDraft} onChange={(event) => setGroupApiKeyDraft(event.target.value)} />
        </FormField>
      </div>}
    </Modal>
    <ConfirmDialog open={deleting !== null} title={t("删除模型")} cancelLabel={t("取消")} confirmLabel={t("删除")} onCancel={() => setDeleting(null)} onConfirm={() => { if (deleting) void appStore.deleteModel(deleting.model_hash); setDeleting(null); }}><p>{t("确定删除这个模型吗？")}</p></ConfirmDialog>
  </>;
}

function Fact({ label, value, tone = "none" }: { label: string; value: string; tone?: "none" | "ok" | "warn" | "bad" }) {
  return <div className={toolbar.fact}>
    <span className={toolbar.factLabel}>{label}</span>
    <span className={toolbar.factValue} data-tone={tone}>{value}</span>
  </div>;
}

function editorTesting(editing: Model | null, testing: Set<string>) {
  return Boolean(editing && testing.has(editing.model_hash));
}

function modelInput(model: Model): ModelInput {
  const { model_hash: _hash, created_at_ms: _created, updated_at_ms: _updated, ...input } = model;
  return input;
}

/** 组内所有模型取值一致时返回该值，否则返回 null（表单留空表示保持不变）。 */
function sharedValue(values: string[]): string | null {
  const [first, ...rest] = values;
  if (first === undefined) return null;
  return rest.every((value) => value === first) ? first : null;
}

function draftInput(draft: CursorModelDraft): ModelInput {
  const model = {
    ...draft.model,
    display_name: draft.model.display_name.trim(),
    base_url: draft.model.base_url.trim(),
    api_key: draft.model.api_key.trim(),
    tooltip_data: draft.model.tooltip_data.trim(),
    model_id: draft.model.model_id.trim(),
    openai_extra_params: parseObject(draft.openAIExtraParamsText, t("OpenAI 额外参数")),
    custom_headers: parseHeaders(draft.customHeadersText),
    anthropic_extra_params: parseObject(draft.anthropicExtraParamsText, t("Anthropic 额外参数")),
  };
  if (!model.display_name || !model.base_url || !model.api_key || !model.tooltip_data || !model.model_id) throw new Error(t("服务器地址或完整请求 URL、API Key、模型名称、显示名称和备注不能为空"));
  for (const [label, value] of [[t("上下文窗口 Token"), model.context_window_tokens], [t("最大输出 Token"), model.type === "openai" ? model.max_completion_tokens : model.anthropic_max_tokens], [t("思考预算 Token"), model.thinking_budget_tokens]] as const) {
    if (value !== null && (!Number.isSafeInteger(value) || value <= 0)) throw new Error(t("{label} 必须是大于 0 的整数", { label }));
  }
  return model;
}

function parseHeaders(text: string): Record<string, string> {
  const parsed = parseObject(text, t("自定义 Headers"));
  if (Object.values(parsed).some((value) => typeof value !== "string")) throw new Error(t("自定义 Headers 的值必须都是字符串"));
  return parsed as Record<string, string>;
}

function parseObject(text: string, label: string): Record<string, unknown> {
  let parsed: unknown;
  try { parsed = JSON.parse(text || "{}"); } catch { throw new Error(t("{label} 必须是有效 JSON", { label })); }
  if (!parsed || Array.isArray(parsed) || typeof parsed !== "object") throw new Error(t("{label} 必须是 JSON 对象", { label }));
  return parsed as Record<string, unknown>;
}

function errorText(cause: unknown) {
  return cause instanceof Error ? cause.message : String(cause);
}
