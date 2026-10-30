import { autoUpdate, computePosition, flip, offset, shift, size } from "@floating-ui/dom";
import { useCallback, useEffect, useId, useLayoutEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { createPortal } from "react-dom";
import { Icon, type IconProps } from "./Icon";
import { checkIcon, chevronDownIcon, windowCloseIcon } from "./icons";
import { VirtualList } from "../virtual/VirtualList";
import type { VirtualListApi } from "../virtual/virtualTypes";
import styles from "./ModelSelect.module.scss";

export type ModelSelectOption = {
  value: string;
  label: string;
  group: string;
  icon?: IconProps["icon"];
  iconSrc?: string;
};

type ModelSelectProps = {
  label: string;
  options: ModelSelectOption[];
  disabled?: boolean;
  /** 多选且为空时的摘要。列表筛选传“全部”，设置表单传“未选择”。 */
  emptySummary?: string;
} & (
  | { mode: "single"; value: string; onChange: (value: string) => void }
  | { mode: "multiple"; value: string[]; onChange: (value: string[]) => void }
);

type MenuItem =
  | { kind: "group"; key: string; label: string; options: ModelSelectOption[] }
  | { kind: "option"; key: string; option: ModelSelectOption };

function groupedItems(options: ModelSelectOption[]): MenuItem[] {
  const groups = new Map<string, ModelSelectOption[]>();
  for (const option of options) {
    const group = groups.get(option.group);
    if (group) group.push(option);
    else groups.set(option.group, [option]);
  }
  return [...groups].flatMap(([group, groupOptions]) => [
    { kind: "group" as const, key: `group:${group}`, label: group, options: groupOptions },
    ...groupOptions.map((option) => ({ kind: "option" as const, key: `option:${option.value}`, option })),
  ]);
}

function GroupCheckbox({ id, active, label, options, selected, onChange }: {
  id: string;
  active: boolean;
  label: string;
  options: ModelSelectOption[];
  selected: Set<string>;
  onChange: (checked: boolean) => void;
}) {
  const input = useRef<HTMLInputElement>(null);
  const selectedCount = options.filter((option) => selected.has(option.value)).length;
  const checked = selectedCount === options.length;
  const indeterminate = selectedCount > 0 && !checked;

  useEffect(() => {
    if (input.current) input.current.indeterminate = indeterminate;
  }, [indeterminate]);

  return <label id={id} className={styles.group} data-active={active || undefined} onMouseDown={(event) => event.preventDefault()}>
    <input
      ref={input}
      type="checkbox"
      tabIndex={-1}
      checked={checked}
      aria-checked={indeterminate ? "mixed" : checked}
      onChange={(event) => onChange(event.target.checked)}
    />
    <span className={styles.checkbox} data-indeterminate={indeterminate || undefined} aria-hidden="true">
      {checked && <Icon icon={checkIcon} size="0.875em" />}
    </span>
    <span>{label}</span>
  </label>;
}

export function ModelSelect(props: ModelSelectProps) {
  const { label, options, disabled, mode } = props;
  const root = useRef<HTMLDivElement>(null);
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const listApi = useRef<VirtualListApi | null>(null);
  const menuId = useId();
  const [open, setOpen] = useState(false);
  const [activeIndex, setActiveIndex] = useState(-1);
  const [position, setPosition] = useState({ left: 0, top: 0, width: 220, maxHeight: 280 });
  const items = useMemo(() => groupedItems(options), [options]);
  const firstOptionIndex = useMemo(() => items.findIndex((item) => item.kind === "option"), [items]);
  const actionable = useMemo(
    () => items.flatMap((item, index) => item.kind === "option" || mode === "multiple" ? [index] : []),
    [items, mode],
  );
  const selected = useMemo(
    () => new Set(mode === "multiple" ? props.value : [props.value]),
    [mode, props.value],
  );

  useLayoutEffect(() => {
    if (!open || !root.current || !menu.current) return;
    return autoUpdate(root.current, menu.current, () => void computePosition(root.current!, menu.current!, {
      placement: "bottom-start",
      middleware: [offset(5), flip({ padding: 10 }), shift({ padding: 10 }), size({
        padding: 10,
        apply: ({ rects, availableHeight }) => setPosition((current) => ({
          ...current,
          width: rects.reference.width,
          maxHeight: Math.max(120, availableHeight),
        })),
      })],
    }).then(({ x, y }) => setPosition((current) => ({ ...current, left: x, top: y }))));
  }, [open]);

  const close = () => {
    setOpen(false);
    trigger.current?.focus();
  };

  useEffect(() => {
    if (!open) return;
    const closeOutside = (event: PointerEvent) => {
      const target = event.target as Node;
      if (!root.current?.contains(target) && !menu.current?.contains(target)) close();
    };
    document.addEventListener("pointerdown", closeOutside);
    return () => document.removeEventListener("pointerdown", closeOutside);
  }, [open]);

  useEffect(() => {
    if (!open || activeIndex < 0) return;
    listApi.current?.scrollToIndex(activeIndex, "auto");
  }, [activeIndex, open]);

  const summary = mode === "multiple"
    ? props.value.length === 0
      ? props.emptySummary ?? t("全部")
      : t("已选 {count} 项", { count: props.value.length })
    : options.find((option) => option.value === props.value)?.label
      ?? (props.value.trim() ? props.value : t("未选择"));

  const choose = (option: ModelSelectOption) => {
    if (mode === "single") {
      props.onChange(option.value);
      close();
      return;
    }
    props.onChange(selected.has(option.value)
      ? props.value.filter((item) => item !== option.value)
      : [...props.value, option.value]);
  };

  const toggleGroup = (groupOptions: ModelSelectOption[], checked: boolean) => {
    if (mode !== "multiple") return;
    const groupValues = new Set(groupOptions.map((option) => option.value));
    props.onChange(checked
      ? [...props.value, ...groupOptions.filter((option) => !selected.has(option.value)).map((option) => option.value)]
      : props.value.filter((value) => !groupValues.has(value)));
  };

  const openMenu = () => {
    if (disabled) return;
    const width = root.current?.getBoundingClientRect().width;
    if (width) setPosition((current) => ({ ...current, width }));
    setActiveIndex(firstOptionIndex);
    setOpen(true);
  };

  const moveActive = (delta: number) => {
    if (actionable.length === 0) return;
    const current = actionable.indexOf(activeIndex);
    const next = current < 0
      ? (delta > 0 ? 0 : actionable.length - 1)
      : (current + delta + actionable.length) % actionable.length;
    setActiveIndex(actionable[next]);
  };

  const activate = (index: number) => {
    const item = items[index];
    if (!item) return;
    if (item.kind === "group") {
      const checked = item.options.every((option) => selected.has(option.value));
      toggleGroup(item.options, !checked);
      return;
    }
    choose(item.option);
  };

  const onTriggerKeyDown = (event: KeyboardEvent<HTMLButtonElement>) => {
    if (!open) {
      if (event.key === "ArrowDown" || event.key === "ArrowUp") {
        event.preventDefault();
        openMenu();
      }
      return;
    }
    if (event.key === "ArrowDown" || event.key === "ArrowUp") {
      event.preventDefault();
      moveActive(event.key === "ArrowDown" ? 1 : -1);
      return;
    }
    if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      if (actionable.length) setActiveIndex(actionable[event.key === "Home" ? 0 : actionable.length - 1]);
      return;
    }
    if (event.key === "Enter" || event.key === " ") {
      event.preventDefault();
      activate(activeIndex);
      return;
    }
    if (event.key === "Escape") {
      event.preventDefault();
      close();
      return;
    }
    if (event.key === "Tab") setOpen(false);
  };

  const rememberList = useCallback((api: VirtualListApi) => {
    listApi.current = api;
  }, []);

  const activeId = open && activeIndex >= 0 ? `${menuId}-item-${activeIndex}` : undefined;

  return <>
    <div ref={root} className={styles.target} data-open={open || undefined} data-disabled={disabled || undefined}>
      <button
        ref={trigger}
        type="button"
        className={styles.trigger}
        disabled={disabled}
        aria-label={t("筛选项：{label}，{summary}", { label, summary })}
        aria-haspopup="listbox"
        role="combobox"
        aria-controls={open ? menuId : undefined}
        aria-expanded={open}
        aria-activedescendant={activeId}
        onClick={() => { if (open) close(); else openMenu(); }}
        onKeyDown={onTriggerKeyDown}
      >
        <span>{label}</span><span className={styles.summary} aria-hidden="true">{summary}</span><Icon icon={chevronDownIcon} size="1.1em" />
      </button>
      {mode === "multiple" && props.value.length > 0 && <button
        type="button"
        className={styles.clear}
        disabled={disabled}
        aria-label={t("清除{label}筛选", { label })}
        onClick={() => props.onChange([])}
      ><Icon icon={windowCloseIcon} size="1.1em" /></button>}
    </div>
    {open && createPortal(<div
      id={menuId}
      ref={menu}
      className={styles.menu}
      aria-label={label}
      style={position}
      onPointerDown={(event) => event.stopPropagation()}
      onKeyDown={(event) => { if (event.key === "Escape") { event.preventDefault(); close(); } }}
    >
      <div
        className={styles.listFrame}
        role="listbox"
        aria-label={label}
        aria-multiselectable={mode === "multiple" ? "true" : undefined}
        style={{ height: Math.min(items.length * 32, Math.max(32, position.maxHeight - (mode === "multiple" ? 45 : 0))) }}
      ><VirtualList
          items={items}
          itemKey="key"
          className={styles.list}
          style={{ height: "100%" }}
          estimatedItemHeight={32}
          overscan={4}
          onReady={rememberList}
          empty={<div className={styles.empty}>{t("暂无选项")}</div>}
        >{(item, index) => item.kind === "group"
          ? mode === "multiple"
            ? <GroupCheckbox
                id={`${menuId}-item-${index}`}
                active={activeIndex === index}
                label={item.label}
                options={item.options}
                selected={selected}
                onChange={(checked) => toggleGroup(item.options, checked)}
              />
            : <div className={styles.groupLabel}>{item.label}</div>
          : <label
              className={styles.optionRow}
              id={`${menuId}-item-${index}`}
              data-active={activeIndex === index || undefined}
              role="option"
              aria-selected={selected.has(item.option.value)}
              tabIndex={-1}
              onMouseDown={(event) => event.preventDefault()}
            >
              <input
                type="checkbox"
                checked={selected.has(item.option.value)}
                tabIndex={-1}
                onChange={() => choose(item.option)}
              />
              <span className={styles.checkbox} aria-hidden="true">
                {selected.has(item.option.value) && <Icon icon={checkIcon} size="0.875em" />}
              </span>
              <span className={styles.option}>
                {(item.option.icon || item.option.iconSrc) && <Icon icon={item.option.icon} src={item.option.iconSrc} />}
                <span>{item.option.label}</span>
              </span>
            </label>}</VirtualList></div>
      {mode === "multiple" && options.length > 0 && <div className={styles.footer}>
        <button type="button" onClick={() => props.onChange(options.map((option) => option.value))}>{t("全选")}</button>
        <button type="button" onClick={() => props.onChange([])}>{t("全不选")}</button>
      </div>}
    </div>, document.body)}
  </>;
}
