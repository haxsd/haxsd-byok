import { useEffect, useMemo, useState } from "react";
import { pluginText, type CurrencyPricing, type ModelTokenPricing, type PricingMode, type TokenPrice } from "../../shared/api";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { useI18n } from "../../i18n/store";
import { Button } from "../../shared/ui/Button";
import { FormField, TextInput } from "../../shared/ui/FormControls";
import { Select } from "../../shared/ui/Select";
import { ModelSelect, type ModelSelectOption } from "../../shared/ui/ModelSelect";
import { TitledCard } from "../../shared/ui/TitledCard";
import { modelProviderName } from "../../shared/utils/modelProvider";
import { claudeIcon, flatColorOrganizationIcon, openAiIcon } from "../../shared/ui/icons";
import { currencyOf, currencySymbol, formatPrice, pricingFor, type Currency } from "../home/metrics/tokenCost";
import { useMessage } from "../../shared/ui/message";
import styles from "./PricingSettingsCard.module.scss";

type PriceDraft = { input_per_million: string; output_per_million: string; cache_read_per_million: string; cache_write_per_million: string };
type PricingDraft = { mode: PricingMode; fixed: PriceDraft; peak: PriceDraft; offPeak: PriceDraft };
const emptyRule = (): ModelTokenPricing => ({ mode: "fixed", cny: null, usd: null });
function toPriceDraft(price?: TokenPrice): PriceDraft {
  return { input_per_million: price ? String(price.input_per_million) : "", output_per_million: price ? String(price.output_per_million) : "", cache_read_per_million: price ? String(price.cache_read_per_million) : "", cache_write_per_million: price ? String(price.cache_write_per_million) : "" };
}
function toDraft(rule: ModelTokenPricing, currency: Currency): PricingDraft {
  const book = pricingFor(rule, currency);
  return { mode: rule.mode, fixed: toPriceDraft(book?.fixed), peak: toPriceDraft(book?.peak), offPeak: toPriceDraft(book?.off_peak) };
}
function toPrice(draft: PriceDraft, group: string): TokenPrice {
  const parse = (key: keyof PriceDraft, label: string) => {
    const value = draft[key];
    const price = Number(value);
    if (!value.trim() || !Number.isFinite(price) || price < 0) throw new Error(t("{label}必须是非负数", { label: `${group} · ${label}` }));
    return price;
  };
  return { input_per_million: parse("input_per_million", t("输入价格（缓存未命中）")), output_per_million: parse("output_per_million", t("输出价格")), cache_read_per_million: parse("cache_read_per_million", t("缓存读取价格（缓存命中）")), cache_write_per_million: parse("cache_write_per_million", t("缓存写入价格")) };
}
function toSettings(draft: PricingDraft, base: ModelTokenPricing, currency: Currency): ModelTokenPricing {
  // Unused time-period fields share the explicitly entered fixed price, not a
  // guessed supplier rate. The other currency remains null until configured.
  const fixed = draft.mode === "fixed" ? toPrice(draft.fixed, t("全时段")) : toPrice(draft.peak, t("高峰时段"));
  const peak = draft.mode === "fixed" ? fixed : toPrice(draft.peak, t("高峰时段"));
  const offPeak = draft.mode === "fixed" ? fixed : toPrice(draft.offPeak, t("低谷时段"));
  const book: CurrencyPricing = { fixed, peak, off_peak: offPeak };
  return { ...base, mode: draft.mode, [currency === "CNY" ? "cny" : "usd"]: book };
}
function PriceFields({ draft, unit, onChange }: { draft: PriceDraft; unit: string; onChange: (next: PriceDraft) => void }) {
  const field = (key: keyof PriceDraft, label: string) => <FormField label={label}><TextInput type="number" min={0} step="any" value={draft[key]} onChange={event => onChange({ ...draft, [key]: event.target.value })} /></FormField>;
  return <>
    {field("input_per_million", t("输入价格（缓存未命中，{unit}/1M）", { unit }))}
    {field("output_per_million", t("输出价格（{unit}/1M）", { unit }))}
    {field("cache_read_per_million", t("缓存读取价格（缓存命中，{unit}/1M）", { unit }))}
    {field("cache_write_per_million", t("缓存写入价格（{unit}/1M）", { unit }))}
  </>;
}
function PriceRows({ price, unit }: { price: TokenPrice; unit: string }) {
  const row = (label: string, value: number) => <div className={styles.row}><strong>{label}</strong><span className={styles.value}>{`${unit}${formatPrice(value)}`}</span></div>;
  return <>{row(t("输入价格（缓存未命中）"), price.input_per_million)}{row(t("输出价格"), price.output_per_million)}{row(t("缓存读取价格（缓存命中）"), price.cache_read_per_million)}{row(t("缓存写入价格"), price.cache_write_per_million)}</>;
}

export function PricingSettingsCard() {
  const { pricing, models, plugins } = useAppStore();
  const { locale } = useI18n();
  const currency = currencyOf(locale);
  const unit = currencySymbol(currency);
  const message = useMessage();
  const options = useMemo(() => {
    const result: ModelSelectOption[] = models.map(model => ({ value: model.model_hash, label: model.display_name || model.model_id, group: modelProviderName(model), icon: model.type === "anthropic" ? claudeIcon : openAiIcon }));
    const seen = new Set(result.map(option => option.value));
    for (const plugin of plugins) for (const provider of plugin.providers) {
      if (!provider.configured) continue;
      for (const model of provider.models) {
        if (!model.enabled || seen.has(model.id)) continue;
        seen.add(model.id);
        result.push({ value: model.id, label: model.displayName, group: pluginText(provider.displayName, locale) || plugin.name, iconSrc: model.icon || undefined, icon: model.icon ? undefined : flatColorOrganizationIcon });
      }
    }
    for (const key of Object.keys(pricing.models)) if (!seen.has(key)) result.push({ value: key, label: key, group: t("已保存的历史模型") });
    return result;
  }, [models, plugins, pricing.models, locale]);
  const [selected, setSelected] = useState("");
  const [draft, setDraft] = useState<PricingDraft>(() => toDraft(emptyRule(), currency));
  const [editingKey, setEditingKey] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const modelHash = options.some(option => option.value === selected) ? selected : options[0]?.value ?? "";
  // 编辑与删除都绑在明确的键上。选中项与列表都可能被别处改动（模型被删、插件下线、
  // 刷新替换了选项），如果按"派生出来的当前项"保存，草稿会落到列表里的另一个模型上。
  const targetKey = editingKey ?? modelHash;
  const editing = editingKey !== null;
  const saved = pricing.models[targetKey];
  const book = saved ? pricingFor(saved, currency) : null;
  useEffect(() => {
    // A language change switches the price book; cancel any draft from the
    // previous currency instead of applying its numeric values to the new one.
    setEditingKey(null);
  }, [currency]);
  useEffect(() => {
    // 正在编辑的模型从列表里消失时取消草稿，而不是改去编辑列表里的第一个模型。
    if (editingKey !== null && !options.some(option => option.value === editingKey)) setEditingKey(null);
  }, [options, editingKey]);
  useEffect(() => { if (!editing) setDraft(toDraft(saved ?? emptyRule(), currency)); }, [saved, currency, editing]);
  const edit = () => { setDraft(toDraft(saved ?? emptyRule(), currency)); setEditingKey(modelHash); };
  const save = async () => {
    try {
      setSaving(true);
      const next = toSettings(draft, saved ?? emptyRule(), currency);
      if (await appStore.updatePricingSettings(targetKey, next)) { setEditingKey(null); message(t("定价设置已保存")); }
      else message.error(appStore.getSnapshot().error ?? t("保存失败"));
    } catch (cause) { message.error(cause); }
    finally { setSaving(false); }
  };
  const remove = async () => {
    try {
      setSaving(true);
      if (await appStore.removePricingSettings(targetKey)) { setEditingKey(null); message(t("已移除此模型定价，相关用量将显示为未定价")); }
      else message.error(appStore.getSnapshot().error ?? t("保存失败"));
    } finally { setSaving(false); }
  };
  const actions = editing ? <div className={styles.actionGroup}>
    <Button size="small" disabled={saving} onClick={() => setEditingKey(null)}>{t("取消")}</Button>
    <Button variant="primary" size="small" disabled={saving || !targetKey} onClick={() => void save()}>{saving ? t("保存中…") : t("保存")}</Button>
  </div> : <div className={styles.actionGroup}>
    {saved && <Button size="small" disabled={saving || !targetKey} onClick={() => void remove()}>{t("移除定价")}</Button>}
    <Button size="small" disabled={saving || !modelHash} onClick={edit}>{t("编辑")}</Button>
  </div>;
  return <TitledCard title={t("模型定价")} action={actions}>
    <div className={styles.content}>
      <small className={styles.rule}>{t("按模型单价估算；未定价用量不计入金额。")}</small>
      <small className={styles.rule}>{t("价格币种跟随界面语言：中文用人民币，其它语言用美元，两套价格各自独立维护。")}</small>
      <ModelSelect mode="single" label={t("选择定价模型")} options={options} value={modelHash} disabled={editing || saving} onChange={setSelected} />
      {options.length === 0 ? <small>{t("请先添加模型，再设置单价。")}</small> : editing ? <div className={styles.fields}>
        <FormField label={t("计价方式")}><Select value={draft.mode} options={[{ value: "fixed", label: t("全时段固定单价") }, { value: "peak_off_peak", label: t("高峰 / 低谷分别计价") }]} ariaLabel={t("计价方式")} onChange={value => setDraft({ ...draft, mode: value as PricingMode })} /></FormField>
        {draft.mode === "fixed" ? <PriceFields draft={draft.fixed} unit={unit} onChange={fixed => setDraft({ ...draft, fixed })} /> : <>
          <span className={styles.sectionTitle}>{t("高峰时段")}</span><PriceFields draft={draft.peak} unit={unit} onChange={peak => setDraft({ ...draft, peak })} />
          <span className={styles.sectionTitle}>{t("低谷时段")}</span><PriceFields draft={draft.offPeak} unit={unit} onChange={offPeak => setDraft({ ...draft, offPeak })} />
        </>}
      </div> : book ? <>
        <div className={styles.row}><strong>{t("计价方式")}</strong><span className={styles.value}>{saved.mode === "fixed" ? t("全时段固定单价") : t("高峰 / 低谷分别计价")}</span></div>
        {saved.mode === "fixed" ? <PriceRows price={book.fixed} unit={unit} /> : <><span className={styles.sectionTitle}>{t("高峰时段")}</span><PriceRows price={book.peak} unit={unit} /><span className={styles.sectionTitle}>{t("低谷时段")}</span><PriceRows price={book.off_peak} unit={unit} /></>}
      </> : <small>{t("此模型在当前币种下未定价，请填写单价。")}</small>}
      {(editing ? draft.mode : saved?.mode) === "peak_off_peak" && <span className={styles.rule}>{t("规则：UTC 周一至周五 01:00-04:00、06:00-10:00 为高峰期，其余时段（含周末）为低谷期；两段单价分别填写。")}</span>}
      <small className={styles.rule}>{t("修改单价会重新估算历史用量；这里不是服务商账单。")}</small>
    </div>
  </TitledCard>;
}
