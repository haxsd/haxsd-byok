import { useId, type ReactNode } from "react";
import { Modal } from "./Modal";

export function ConfirmDialog({ id, open, title, children, busy, wide, destructive = false, cancelLabel = t("取消"), confirmLabel = t("确认"), onCancel, onConfirm }: {
  id?: string;
  open: boolean;
  title: string;
  children: ReactNode;
  busy?: boolean;
  wide?: boolean;
  /** 删除、清空、消耗类操作默认把焦点放在「取消」上，避免回车误执行。 */
  destructive?: boolean;
  cancelLabel?: string;
  confirmLabel?: string;
  onCancel: () => void;
  onConfirm: () => void;
}) {
  const contentId = useId();
  return <Modal
    id={id}
    open={open}
    title={title}
    busy={busy}
    wide={wide}
    role="alertdialog"
    ariaDescribedBy={contentId}
    initialFocus={destructive ? "first" : "submit"}
    closeLabel={cancelLabel}
    submitLabel={confirmLabel}
    onClose={onCancel}
    onSubmit={onConfirm}
  >
    <div id={contentId}>{children}</div>
  </Modal>;
}
