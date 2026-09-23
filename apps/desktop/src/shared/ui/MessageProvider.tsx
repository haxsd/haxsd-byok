import { useSyncExternalStore } from "react";
import { createPortal } from "react-dom";
import { Icon } from "./Icon";
import { alertCircleIcon, informationOutlineIcon } from "./icons";
import { getMessageSnapshot, subscribeToMessages } from "./message";
import styles from "./MessageProvider.module.scss";

/**
 * 顶部的提示条。
 *
 * 之前的形态是一行居中文字，成功与失败长得完全一样——「连通性测试失败」和「设置已保存」
 * 只有读完才知道是哪一种。现在失败是红色的、带警示图标、停留更久，成功/信息保持中性。
 */
export function MessageProvider() {
  const { current } = useSyncExternalStore(subscribeToMessages, getMessageSnapshot);

  return createPortal(
    <div className={styles.region} aria-live="polite" aria-atomic="true">
      {current && (
        <div
          key={current.id}
          className={`${styles.message} ${current.leaving ? styles.leaving : ""}`}
          data-tone={current.tone}
          role={current.tone === "error" ? "alert" : "status"}
        >
          <Icon icon={current.tone === "error" ? alertCircleIcon : informationOutlineIcon} size="1.15em" />
          <span className={styles.text}>{current.content}</span>
        </div>
      )}
    </div>,
    document.body,
  );
}
