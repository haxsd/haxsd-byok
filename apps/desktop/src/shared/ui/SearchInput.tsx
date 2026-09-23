import type { InputHTMLAttributes } from "react";
import { Icon } from "./Icon";
import { closeIcon, searchIcon } from "./icons";
import styles from "./SearchInput.module.scss";

/**
 * The filter input used by every list page. It carries its own clear button, which
 * is the part that used to be missing: a filter you cannot see the value of, or
 * clear without selecting the text, is a filter people stop using.
 */
export function SearchInput({ value, onValueChange, ariaLabel, placeholder, className, ...props }: {
  value: string;
  onValueChange: (value: string) => void;
  ariaLabel: string;
  placeholder?: string;
} & Omit<InputHTMLAttributes<HTMLInputElement>, "value" | "onChange" | "aria-label" | "className"> & { className?: string }) {
  return <div className={[styles.root, className].filter(Boolean).join(" ")}>
    <Icon icon={searchIcon} size="1.05em" className={styles.icon} />
    <input
      {...props}
      type="search"
      value={value}
      aria-label={ariaLabel}
      placeholder={placeholder}
      className={styles.input}
      onChange={(event) => onValueChange(event.target.value)}
    />
    {value.length > 0 && <button
      type="button"
      className={styles.clear}
      aria-label={t("清除筛选")}
      onClick={() => onValueChange("")}
    ><Icon icon={closeIcon} size="1em" /></button>}
  </div>;
}
