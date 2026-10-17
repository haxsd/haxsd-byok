import { memo, useEffect, useId, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { useNavigate } from "react-router-dom";
import { useI18n } from "../i18n/store";
import { useAppStoreSelector } from "../shared/store/appStore";
import { selectCalls, selectModels, selectTheme } from "../shared/store/selectors";
import { Icon } from "../shared/ui/Icon";
import { arrowRightIcon, searchIcon } from "../shared/ui/icons";
import { commandPaletteCommands, type PaletteCommand as Command } from "./commandPaletteCommands";
import { paletteMatches } from "./commandPaletteMatches";
import styles from "./CommandPalette.module.scss";

/**
 * 命令面板（Ctrl/⌘ + K）。
 *
 * 存在的理由是「找东西」：模型和调用记录以前只能靠翻页面，而调用 ID 在界面上根本
 * 没有入口可以搜。这里把三件事放在同一个输入框里——跳页面、执行动作、按名字或 ID
 * 找到一次调用。
 *
 * 命令表本身在 `commandPaletteCommands.ts`：那些 `run` 是动作，失败必须有人接，
 * 放进独立模块才能用假服务端把「点了之后 store 里有没有错误」钉住。
 */
export const CommandPalette = memo(function CommandPalette() {
  const [open, setOpen] = useState(false);
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const navigate = useNavigate();
  const locale = useI18n().locale;
  // 三个字段分别订阅：命令表只在这三者变化时重建，别的一次写入（busy、error、某个页面的
  // 状态）都不该惊动这个常驻组件。
  const models = useAppStoreSelector(selectModels);
  const calls = useAppStoreSelector(selectCalls);
  const theme = useAppStoreSelector(selectTheme);
  const inputRef = useRef<HTMLInputElement>(null);
  const listRef = useRef<HTMLDivElement>(null);
  const listId = useId();

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

  // 命令面板是模态的：打开时把应用根节点设为 inert，Tab 与点击都留在面板里。
  // 否则 Tab 会把焦点带到面板后面的页面上（焦点看不见，回车还会触发那个按钮）。
  useEffect(() => {
    if (!open) return;
    const root = document.getElementById("root");
    if (!root) return;
    root.setAttribute("inert", "");
    return () => root.removeAttribute("inert");
  }, [open]);

  const commands = useMemo<Command[]>(() => commandPaletteCommands({
    calls,
    models,
    locale,
    theme,
    navigate: (path) => void navigate(path),
    close: () => setOpen(false),
  }), [calls, locale, models, navigate, theme]);

  const matches = useMemo(() => paletteMatches(commands, query), [commands, query]);

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
            role="combobox"
            aria-expanded={matches.length > 0}
            aria-controls={listId}
            aria-autocomplete="list"
            aria-activedescendant={matches[active] ? `${listId}-option-${active}` : undefined}
            onChange={(event) => { setQuery(event.target.value); setActive(0); }}
            onKeyDown={(event) => {
              if (event.key === "ArrowDown") { event.preventDefault(); setActive((index) => Math.min(matches.length - 1, index + 1)); }
              if (event.key === "ArrowUp") { event.preventDefault(); setActive((index) => Math.max(0, index - 1)); }
              if (event.key === "Enter" && matches[active]) { event.preventDefault(); matches[active].run(); }
            }}
          />
          <kbd className={styles.kbd}>Esc</kbd>
        </div>
        <div className={styles.list} ref={listRef} role="listbox" id={listId} aria-label={t("命令")}>
          {matches.length === 0 && <div className={styles.empty}>{t("没有匹配的命令")}</div>}
          {groups.map((section) => <div key={section.group} className={styles.group} role="group" aria-label={section.group} data-command-group>
            <span className={styles.groupLabel}>{section.group}</span>
            {section.items.map(({ command, index }) => <button
              key={command.id}
              type="button"
              role="option"
              id={`${listId}-option-${index}`}
              aria-selected={index === active}
              /* 焦点始终留在输入框上，选项由 aria-activedescendant 指出来：
                 combobox 的 ↑/↓/回车都由输入框处理，Tab 不该停在选项上。 */
              tabIndex={-1}
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
});
