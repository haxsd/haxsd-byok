import type { IconifyIcon } from "@iconify/react";
import { Icon } from "./Icon";
import styles from "./Segmented.module.scss";

export type SegmentedOption<T extends string> = {
  value: T;
  label: string;
  icon?: IconifyIcon;
  title?: string;
};

/**
 * A segmented control for choices that are all visible at once: a time range, a
 * grouping, a filter state. The same decisions used to be spread across three
 * shapes — a row of pills in the page actions, a bordered button strip inside a
 * card, and a Select — depending on which page you were on.
 */
export function Segmented<T extends string>({ value, options, ariaLabel, disabled, onChange }: {
  value: T;
  options: SegmentedOption<T>[];
  ariaLabel: string;
  disabled?: boolean;
  onChange: (value: T) => void;
}) {
  return <div className={styles.root} role="radiogroup" aria-label={ariaLabel}>
    {options.map((option) => <button
      key={option.value}
      type="button"
      role="radio"
      aria-checked={option.value === value}
      aria-label={option.label}
      title={option.title ?? option.label}
      disabled={disabled}
      className={styles.option}
      onClick={() => { if (option.value !== value) onChange(option.value); }}
    >
      {option.icon && <Icon icon={option.icon} size="1.05em" />}
      <span>{option.label}</span>
    </button>)}
  </div>;
}
