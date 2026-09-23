import type { IconifyIcon } from "@iconify/react";
import { useEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useNavigate } from "react-router-dom";
import cursorIconUrl from "../shared/assets/icons/cursor.svg";
import { api } from "../shared/api";
import { useI18n } from "../i18n/store";
import { appStore, useAppStore } from "../shared/store/appStore";
import { Icon } from "../shared/ui/Icon";
import { themeOptions } from "../shared/theme/theme";
import { navCallsIcon, navDevinIcon, navModelsIcon, navOverviewIcon, navPluginsIcon, navSettingsIcon, navTutorialIcon } from "../shared/ui/navIcons";
import { activityIcon, arrowRightIcon, eyeIcon, refreshIcon, searchIcon } from "../shared/ui/icons";
import styles from "./CommandPalette.module.scss";

type Command = {
  id: string;
  group: string;
  label: string;
  hint?: string;
  icon: IconifyIcon | string;
  keywords?: string;
  run: () => void;
};

/**
 * 命令面板（Ctrl/⌘ + K）。
 *
 * 存在的理由是「找东西」：模型和调用记录以前只能靠翻页面，而调用 ID 在界面上根本
 * 没有入口可以搜。这里把三件事放在同一个输入框里——跳页面、执行动作、按名字或 ID
 * 找到一次调用。
 */
export function CommandPalette() {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const navigate = useNavigate();
  const { locale } = useI18n();
  const { models, calls, theme } = useAppStore();
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === "k") {
        event.preventDefault();
        setOpen((current) => !current);
        return;
      }
      if (event.key === "Escape") setOpen(false);
    };
    window.addEventListener("keydown", onKeyDown);
    // The button in the navigation bar opens it too: a shortcut nobody can discover
    // is a shortcut only its author uses.
    const onOpenRequest = () => setOpen(true);
    window.addEventListener("byok:open-command-palette", onOpenRequest);
    return () => {
      window.removeEventListener("keydown", onKeyDown);
      window.removeEventListener("byok:open-command-palette", onOpenRequest);
    };
  }, []);

  useEffect(() => {
    if (!open) return;
    setQuery("");
    setActive(0);
    const frame = window.requestAnimationFrame(() => inputRef.current?.focus());
    return () => window.cancelAnimationFrame(frame);
  }, [open]);

  const commands = useMemo<Command[]>(() => {
    const go = (path: string) => () => { void navigate(path); setOpen(false); };
    const pages: Command[] = [
      { id: "page-overview", group: t("页面"), label: t("概览"), icon: navOverviewIcon, run: go("/") },
      { id: "page-calls", group: t("页面"), label: t("调用"), icon: navCallsIcon, run: go("/calls") },
      { id: "page-cursor", group: t("页面"), label: "Cursor", icon: cursorIconUrl, run: go("/harness/cursor") },
      { id: "page-devin", group: t("页面"), label: "Devin", icon: navDevinIcon, run: go("/harness/devin") },
      { id: "page-models", group: t("页面"), label: t("模型库"), icon: navModelsIcon, run: go("/models") },
      { id: "page-plugins", group: t("页面"), label: t("插件配置"), icon: navPluginsIcon, run: go("/plugins") },
      { id: "page-settings", group: t("页面"), label: t("系统设置"), icon: navSettingsIcon, run: go("/settings") },
      { id: "page-tutorial", group: t("页面"), label: t("使用教程"), icon: navTutorialIcon, run: go("/tutorial") },
    ];
    const actions: Command[] = [
      {
        id: "action-refresh",
        group: t("动作"),
        label: t("刷新数据"),
        hint: t("重新读取模型、调用与状态"),
        icon: refreshIcon,
        run: () => { void appStore.refresh(); setOpen(false); },
      },
      ...themeOptions.map((option) => ({
        id: `theme-${option.id}`,
        group: t("动作"),
        label: t("切换到{name}", { name: themeName(option.id) }),
        hint: option.id === theme ? t("当前主题") : undefined,
        icon: navSettingsIcon,
        keywords: option.id,
        run: () => { appStore.selectTheme(option.id); setOpen(false); },
      })),
      {
        id: "action-tutorial",
        group: t("动作"),
        // 教程是这个应用自己的一页，不是上游产品的文档站：打开外部网页会把用户带到
        // 另一套界面和步骤上去。
        label: t("打开使用教程"),
        icon: navTutorialIcon,
        run: go("/tutorial"),
      },
    ];
    const modelCommands: Command[] = models.slice(0, 8).map((model) => ({
      id: `model-${model.model_hash}`,
      group: t("模型"),
      label: model.display_name,
      hint: model.model_id,
      icon: navModelsIcon,
      keywords: `${model.model_id} ${model.base_url}`,
      run: () => { void navigate("/models"); setOpen(false); },
    }));
    const callCommands: Command[] = calls.slice(0, 40).map((call) => ({
      id: `call-${call.call_id}`,
      group: t("调用"),
      label: call.display_name || call.model_id,
      hint: `${new Date(call.created_at_ms).toLocaleString(locale)} · ${call.status}`,
      icon: call.status === "completed" ? activityIcon : eyeIcon,
      keywords: `${call.call_id} ${call.conversation_id} ${call.model_id}`,
      run: () => { void api.openCallDetails(call.call_id); setOpen(false); },
    }));
    return [...pages, ...actions, ...modelCommands, ...callCommands];
  }, [calls, locale, models, navigate, theme]);

  const matches = useMemo(() => {
    const trimmed = query.trim().toLowerCase();
    if (!trimmed) {
      // 空查询只给页面、动作和模型：调用记录可能有几百条，默认全列出来等于没有重点。
      return commands.filter((command) => command.group !== t("调用")).slice(0, 12);
    }
    return commands
      .map((command) => ({ command, score: matchScore(command, trimmed) }))
      .filter((entry) => entry.score > 0)
      .sort((left, right) => right.score - left.score)
      .slice(0, 14)
      .map((entry) => entry.command);
  }, [commands, locale, query]);

  useEffect(() => {
    if (active < matches.length) return;
    setActive(0);
  }, [active, matches.length]);

  useEffect(() => {
    // Scroll the whole group into view, not just the row: scrolling to the row left
    // the group's own label clipped above the top edge.
    const row = listRef.current?.querySelector<HTMLElement>(`[data-index="${active}"]`);
    (row?.closest("[data-command-group]") ?? row)?.scrollIntoView({ block: "nearest" });
  }, [active]);

  if (!open) return null;

  const groups: Array<{ group: string; items: Array<{ command: Command; index: number }> }> = [];
  matches.forEach((command, index) => {
    const last = groups.at(-1);
    if (last && last.group === command.group) last.items.push({ command, index });
    else groups.push({ group: command.group, items: [{ command, index }] });
  });

  return createPortal(
    <div className={styles.mask} onPointerDown={(event) => { if (event.target === event.currentTarget) setOpen(false); }}>
      <div className={styles.panel} role="dialog" aria-modal="true" aria-label={t("命令面板")}>
        <div className={styles.search}>
          <Icon icon={searchIcon} size="1.15em" />
          <input
            ref={inputRef}
            value={query}
            placeholder={t("搜索页面、动作、模型或调用 ID…")}
            aria-label={t("命令面板搜索")}
            onChange={(event) => { setQuery(event.target.value); setActive(0); }}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown") { event.preventDefault(); setActive((index) => Math.min(matches.length - 1, index + 1)); }
              if (event.key === "ArrowUp") { event.preventDefault(); setActive((index) => Math.max(0, index - 1)); }
              if (event.key === "Enter" && matches[active]) { event.preventDefault(); matches[active].run(); }
            }}
          />
          <kbd className={styles.kbd}>Esc</kbd>
        </div>
        <div className={styles.list} ref={listRef} role="listbox" aria-label={t("命令")}>
          {matches.length === 0 && <div className={styles.empty}>{t("没有匹配的命令")}</div>}
          {groups.map((section) => <div key={section.group} className={styles.group} data-command-group>
            <span className={styles.groupLabel}>{section.group}</span>
            {section.items.map(({ command, index }) => <button
              key={command.id}
              type="button"
              role="option"
              aria-selected={index === active}
              data-index={index}
              className={styles.item}
              data-active={index === active || undefined}
              onMouseEnter={() => setActive(index)}
              onClick={() => command.run()}
            >
              {typeof command.icon === "string"
                ? <Icon src={command.icon} size="1.05em" />
                : <Icon icon={command.icon} size="1.05em" />}
              <span className={styles.itemLabel}>{command.label}</span>
              {command.hint && <span className={styles.itemHint}>{command.hint}</span>}
              <Icon icon={arrowRightIcon} size="1em" className={styles.itemArrow} />
            </button>)}
          </div>)}
        </div>
        <div className={styles.footer}>
          <span><kbd className={styles.kbd}>↑</kbd><kbd className={styles.kbd}>↓</kbd>{t("选择")}</span>
          <span><kbd className={styles.kbd}>Enter</kbd>{t("执行")}</span>
          <span><kbd className={styles.kbd}>Ctrl</kbd><kbd className={styles.kbd}>K</kbd>{t("开关")}</span>
        </div>
      </div>
    </div>,
    document.body,
  );
}

function themeName(id: string) {
  if (id === "midnight") return t("午夜靛蓝");
  if (id === "default-dark") return t("默认暗色");
  return t("默认亮色");
}

/**
 * 命中越靠前分越高，全部字符按顺序出现也算命中（"dvn" 能命中 "Devin"）。
 * 返回 0 表示不命中。
 */
function matchScore(command: Command, query: string) {
  const haystack = `${command.label} ${command.hint ?? ""} ${command.keywords ?? ""}`.toLowerCase();
  const label = command.label.toLowerCase();
  if (label.startsWith(query)) return 100;
  if (label.includes(query)) return 80;
  if (haystack.includes(query)) return 60;
  let position = 0;
  for (const character of query) {
    position = label.indexOf(character, position);
    if (position < 0) return 0;
    position += 1;
  }
  return 20;
}
