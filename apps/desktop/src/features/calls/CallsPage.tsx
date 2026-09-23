import { useEffect, useMemo, useState } from "react";
import type { LlmCall } from "../../shared/api";
import { CallTable } from "./CallTable";
import { PageContent } from "../../shell/layout/PageContent";
import { appStore, useAppStore } from "../../shared/store/appStore";
import { PageTitle } from "../../shared/ui/PageTitle";
import { SearchInput } from "../../shared/ui/SearchInput";
import { Segmented } from "../../shared/ui/Segmented";
import { Select } from "../../shared/ui/Select";
import { ServiceOfflineState } from "../../shared/ui/ServiceOfflineState";
import { Card } from "../../shared/ui/Card";
import { formatCompactInteger } from "../../shared/utils/numberFormat";
import { formatDuration } from "../../shared/utils/relativeTime";
import toolbar from "../../shared/ui/Toolbar.module.scss";
import styles from "./CallsPage.module.scss";

const CALL_REFRESH_INTERVAL_MS = 2_000;

type StatusFilter = "all" | "completed" | "failed" | "other";
type RouteFilter = "all" | "byok" | "official";

export function CallsPage() {
  const { calls, offline } = useAppStore();
  const [search, setSearch] = useState("");
  const [status, setStatus] = useState<StatusFilter>("all");
  const [route, setRoute] = useState<RouteFilter>("all");
  const [model, setModel] = useState("all");

  useEffect(() => {
    let disposed = false;
    const refreshCalls = () => {
      if (!disposed && document.visibilityState === "visible") {
        void appStore.refreshCalls();
      }
    };

    refreshCalls();
    const interval = window.setInterval(refreshCalls, CALL_REFRESH_INTERVAL_MS);
    window.addEventListener("focus", refreshCalls);
    document.addEventListener("visibilitychange", refreshCalls);
    return () => {
      disposed = true;
      window.clearInterval(interval);
      window.removeEventListener("focus", refreshCalls);
      document.removeEventListener("visibilitychange", refreshCalls);
    };
  }, []);

  const modelOptions = useMemo(() => {
    const seen = new Map<string, string>();
    for (const call of calls) {
      const key = call.model_hash ?? call.model_id;
      if (!seen.has(key)) seen.set(key, call.display_name || call.model_id);
    }
    return [...seen.entries()].map(([value, label]) => ({ value, label }));
  }, [calls]);

  const filtered = useMemo(() => {
    const query = search.trim().toLowerCase();
    return calls.filter((call) => {
      if (status === "completed" && call.status !== "completed") return false;
      if (status === "failed" && call.status !== "failed" && call.status !== "error") return false;
      if (status === "other" && (call.status === "completed" || call.status === "failed" || call.status === "error")) return false;
      if (route === "byok" && call.route !== "local_byok") return false;
      if (route === "official" && call.route !== "cursor_official") return false;
      if (model !== "all" && (call.model_hash ?? call.model_id) !== model) return false;
      if (!query) return true;
      return [call.display_name, call.model_id, call.call_id, call.conversation_id, call.status]
        .some((field) => field.toLowerCase().includes(query));
    });
  }, [calls, model, route, search, status]);

  const summary = useMemo(() => summarize(filtered), [filtered]);
  const filteredOut = filtered.length !== calls.length;

  const content = <div className={styles.page}>
    <Card className={styles.summary}>
      <div className={toolbar.facts}>
        <Fact label={t("调用")} value={formatCompactInteger(summary.calls)} />
        <Fact label={t("成功率")} value={summary.successRate === null ? "-" : `${(summary.successRate * 100).toFixed(1)}%`} tone={summary.successRate === null ? "none" : summary.successRate >= 0.95 ? "ok" : "warn"} />
        <Fact label={t("平均耗时")} value={formatDuration(summary.averageDuration)} />
        <Fact label={t("平均 TTFB")} value={formatDuration(summary.averageTtfb)} />
        <Fact label="Token" value={formatCompactInteger(summary.tokens)} />
      </div>
    </Card>
    <div className={styles.toolbar}>
      <SearchInput
        className={styles.search}
        value={search}
        onValueChange={setSearch}
        ariaLabel={t("搜索调用")}
        placeholder={t("搜模型、调用 ID、会话 ID…")}
      />
      <Segmented
        ariaLabel={t("按状态筛选")}
        value={status}
        options={[
          { value: "all", label: t("全部状态") },
          { value: "completed", label: t("成功") },
          { value: "failed", label: t("失败") },
          { value: "other", label: t("其他") },
        ]}
        onChange={setStatus}
      />
      <Segmented
        ariaLabel={t("按路由筛选")}
        value={route}
        options={[
          { value: "all", label: t("全部路由") },
          { value: "byok", label: "BYOK" },
          { value: "official", label: t("官方") },
        ]}
        onChange={setRoute}
      />
      {modelOptions.length > 1 && <div className={styles.modelFilter}><Select
        ariaLabel={t("按模型筛选")}
        value={model === "all" || modelOptions.some((option) => option.value === model) ? model : "all"}
        options={[{ value: "all", label: t("全部模型") }, ...modelOptions]}
        onChange={setModel}
      /></div>}
      <span className={toolbar.spacer} />
      <span className={toolbar.count}>
        {filteredOut ? t("筛出 {count} / {total} 条", { count: filtered.length, total: calls.length }) : t("共 {count} 条", { count: calls.length })}
      </span>
    </div>
    <div className={styles.tableRegion}>
      {offline && calls.length === 0
        ? <div className={styles.offline}><ServiceOfflineState /></div>
        : <CallTable calls={filtered} onDetails={(call) => void appStore.openCallDetails(call.call_id)} />}
    </div>
  </div>;

  return <PageContent
    fixed
    title={<PageTitle title={t("调用")} meta={t("最近 {count} 条实时记录，每 2 秒刷新", { count: calls.length })} />}
    contentClassName={styles.pageContent}
    sections={[{ key: "calls", estimatedHeight: 720, content }]}
  />;
}

function Fact({ label, value, tone = "none" }: { label: string; value: string; tone?: "none" | "ok" | "warn" | "bad" }) {
  return <div className={toolbar.fact}>
    <span className={toolbar.factLabel}>{label}</span>
    <span className={toolbar.factValue} data-tone={tone}>{value}</span>
  </div>;
}

/**
 * 统计的是筛选后的那一批，不是全部历史：筛选条和数字在同一屏上，它们必须说同一件事。
 */
function summarize(calls: LlmCall[]) {
  const durations = calls.map((call) => call.duration_ms).filter((value): value is number => value !== null);
  const ttfb = calls.map((call) => call.ttfb_ms).filter((value): value is number => value !== null);
  const successful = calls.filter((call) => call.status === "completed").length;
  const tokens = calls.reduce((total, call) => total + (call.total_tokens ?? (call.input_tokens ?? 0) + (call.output_tokens ?? 0)), 0);
  return {
    calls: calls.length,
    successRate: calls.length > 0 ? successful / calls.length : null,
    averageDuration: durations.length > 0 ? durations.reduce((sum, value) => sum + value, 0) / durations.length : null,
    averageTtfb: ttfb.length > 0 ? ttfb.reduce((sum, value) => sum + value, 0) / ttfb.length : null,
    tokens,
  };
}
