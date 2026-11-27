import type { ReactNode } from "react";
import { useAppStoreSelector } from "../../shared/store/appStore";
import styles from "./SettingsLayers.module.css";

/**
 * Client settings are split into three layers:
 *
 * 1. the normal card: status, model and the primary action;
 * 2. optional advanced configuration: ports, paths and connection details;
 * 3. developer diagnostics: internal hashes, raw files and protocol fields.
 *
 * These components keep that rule in one place. Pages should not read the
 * developer-mode flag themselves just to decide whether to render a detail.
 */
export function AdvancedSettings({ title = t("高级配置"), description, children, className }: {
  title?: string;
  description?: string;
  children: ReactNode;
  className?: string;
}) {
  return <details className={[styles.layer, className].filter(Boolean).join(" ")}>
    <summary>
      <strong>{title}</strong>
      {description && <span>{description}</span>}
    </summary>
    <div className={styles.body}>{children}</div>
  </details>;
}

export function DeveloperSettings({
  title = t("开发者诊断"),
  description,
  forceVisible = false,
  children,
  className,
}: {
  title?: string;
  description?: string;
  /** 出错时允许普通模式显示诊断；正常的设备信息仍然只在开发者模式出现。 */
  forceVisible?: boolean;
  children: ReactNode;
  className?: string;
}) {
  const developerMode = useAppStoreSelector((state) => state.developerMode);
  if (!developerMode && !forceVisible) return null;
  return <AdvancedSettings title={title} description={description} className={className}>
    {children}
  </AdvancedSettings>;
}

export function SettingsGroup({ title, description, children }: {
  title: string;
  description?: string;
  children: ReactNode;
}) {
  return <section className={styles.group}>
    <div className={styles.groupHeading}>
      <strong>{title}</strong>
      {description && <small>{description}</small>}
    </div>
    {children}
  </section>;
}
