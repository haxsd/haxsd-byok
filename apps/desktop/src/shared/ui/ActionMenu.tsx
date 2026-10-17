import { autoUpdate, computePosition, flip, offset, shift } from "@floating-ui/dom";
import type { IconifyIcon } from "@iconify/react";
import { useCallback, useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import { createPortal } from "react-dom";
import { Button } from "./Button";
import { Icon } from "./Icon";
import { chevronDownIcon } from "./icons";
import styles from "./ActionMenu.module.scss";

export type ActionMenuItem =
  | {
      id: string;
      label: string;
      type?: "action";
      disabled?: boolean;
      onSelect: () => void;
    }
  | {
      id: string;
      label: string;
      type: "text";
    };

/** 能接焦点的一项：动作项、且没被禁用。分隔文字不参与。 */
function isFocusable(item: ActionMenuItem | undefined): boolean {
  return item !== undefined && item.type !== "text" && !item.disabled;
}

/**
 * 菜单里的第一项、以及上/下一项。
 *
 * 抽成纯函数是为了能直接测：菜单挂在 body 上，键盘用户既不能用 Tab 进到菜单里
 * （Tab 会跳到页面里的下一个控件），也不能用方向键移动焦点——「编辑 / 复制 / 删除」
 * 于是只剩鼠标可达（实测确认过）。这里的绕行与跳过禁用项是那段行为里最容易写错的
 * 部分，所以单独可测。
 */
export function firstMenuItemIndex(items: ActionMenuItem[]): number {
  return items.findIndex((item) => isFocusable(item));
}

export function nextMenuItemIndex(items: ActionMenuItem[], current: number, step: number): number {
  if (items.length === 0) return -1;
  const start = current < 0 ? (step > 0 ? -1 : 0) : current;
  for (let offset = 1; offset <= items.length; offset += 1) {
    const index = ((start + step * offset) % items.length + items.length) % items.length;
    if (isFocusable(items[index])) return index;
  }
  return -1;
}

/** 触发器 + 动作列表的下拉菜单,用于容纳卡片上的次要操作。 */
export function ActionMenu({ label, items, disabled, icon, quiet = false }: {
  label: string;
  items: ActionMenuItem[];
  disabled?: boolean;
  /** 传图标时触发器只显示图标，label 转为无障碍名称。 */
  icon?: IconifyIcon;
  /** 安静形态：无边框的图标按钮，用在密集的卡片角上。 */
  quiet?: boolean;
}) {
  const trigger = useRef<HTMLButtonElement>(null);
  const menu = useRef<HTMLDivElement>(null);
  const itemRefs = useRef<Array<HTMLButtonElement | null>>([]);
  const menuId = useId();
  const [open, setOpen] = useState(false);
  const [position, setPosition] = useState({ left: 0, top: 0 });

  useLayoutEffect(() => {
    if (!open || !trigger.current || !menu.current) return;
    return autoUpdate(trigger.current, menu.current, () =>
      void computePosition(trigger.current!, menu.current!, {
        placement: "bottom-end",
        middleware: [offset(5), flip({ padding: 10 }), shift({ padding: 10 })],
      }).then(({ x, y }) => setPosition({ left: x, top: y })));
  }, [open]);

  // 打开就把焦点交给第一项：菜单挂在 body 末尾，不主动接管焦点的话，按 Tab 只会从
  // 触发器跳到页面里的下一个控件，菜单项永远够不到。
  useLayoutEffect(() => {
    if (!open) return;
    const first = firstMenuItemIndex(items);
    if (first >= 0) itemRefs.current[first]?.focus();
    // items 变化不需要重新抢焦点：菜单打开期间只有点击与键盘两条路径。
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const outside = (event: PointerEvent) => {
      if (!trigger.current?.contains(event.target as Node) && !menu.current?.contains(event.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("pointerdown", outside);
    return () => document.removeEventListener("pointerdown", outside);
  }, [open]);

  const close = useCallback(() => {
    setOpen(false);
    trigger.current?.focus();
  }, []);

  const moveFocus = useCallback((current: number, step: number) => {
    const next = nextMenuItemIndex(items, current, step);
    if (next >= 0) itemRefs.current[next]?.focus();
  }, [items]);

  return <>
    <Button
      ref={trigger}
      size="small"
      disabled={disabled}
      aria-label={icon ? label : undefined}
      aria-haspopup="menu"
      aria-controls={open ? menuId : undefined}
      aria-expanded={open}
      data-quiet={quiet || undefined}
      className={icon ? styles.iconTrigger : undefined}
      onClick={() => setOpen((current) => !current)}
      onKeyDown={(event) => {
        if (event.key === "Escape") close();
        if (event.key === "ArrowDown" || event.key === "ArrowUp") {
          event.preventDefault();
          setOpen(true);
        }
      }}
    >
      {icon ? <Icon icon={icon} size="1.15em" /> : label}
      {!icon && <Icon icon={chevronDownIcon} size="1em" className={open ? styles.openIcon : undefined} />}
    </Button>
    {open && createPortal(
      <div
        id={menuId}
        ref={menu}
        className={styles.menu}
        role="menu"
        style={{ left: position.left, top: position.top }}
        onKeyDown={(event) => {
          const current = itemRefs.current.findIndex((node) => node === document.activeElement);
          switch (event.key) {
            case "Escape":
              event.preventDefault();
              close();
              break;
            case "ArrowDown":
              event.preventDefault();
              moveFocus(current, 1);
              break;
            case "ArrowUp":
              event.preventDefault();
              moveFocus(current, -1);
              break;
            case "Home":
              event.preventDefault();
              moveFocus(-1, 1);
              break;
            case "End":
              event.preventDefault();
              moveFocus(0, -1);
              break;
            case "Tab":
              // 菜单在 body 末尾，默认的 Tab 会跳到文档之外；先把焦点还给触发器，
              // 用户的下一次 Tab 就从这里继续。
              event.preventDefault();
              close();
              break;
            default:
              break;
          }
        }}
      >
        {items.map((item, index) => item.type === "text" ? (
          <span key={item.id} className={styles.textItem}>
            {item.label}
          </span>
        ) : (
          <button
            key={item.id}
            ref={(node) => { itemRefs.current[index] = node; }}
            type="button"
            role="menuitem"
            tabIndex={-1}
            disabled={item.disabled}
            onMouseEnter={(event) => event.currentTarget.focus()}
            onClick={() => {
              setOpen(false);
              // 焦点跟着动作走：菜单卸掉后焦点会掉到 body，键盘用户要从头 Tab。
              trigger.current?.focus();
              item.onSelect();
            }}
          >
            {item.label}
          </button>
        ))}
      </div>,
      document.body,
    )}
  </>;
}
