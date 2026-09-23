const MIN_VISIBLE_MS = 300;
const EXIT_MS = 200;
/** 失败信息停留更久：错误通常比确认信息更长，读完需要时间。 */
const ERROR_DURATION_MS = 6000;

export type MessageTone = "info" | "error";

export type MessageOptions = {
  duration?: number;
  tone?: MessageTone;
};

export type MessageItem = {
  id: string;
  content: string;
  tone: MessageTone;
  leaving: boolean;
};

type MessageSnapshot = {
  current: MessageItem | null;
};

type MessageApi = ((content: unknown, options?: MessageOptions) => string | null) & {
  remove: (id: string) => void;
  clear: () => void;
  /** 失败信息：红色、带图标、停留更久。 */
  error: (cause: unknown, options?: MessageOptions) => string | null;
};

let seed = 0;
let shownAt = 0;
let dismissTimer: number | null = null;
let exitTimer: number | null = null;
let snapshot: MessageSnapshot = { current: null };
const listeners = new Set<() => void>();

function update(current: MessageItem | null) {
  snapshot = { current };
  listeners.forEach((listener) => listener());
}

function clearTimer(timer: number | null) {
  if (timer !== null) window.clearTimeout(timer);
}

function clearTimers() {
  clearTimer(dismissTimer);
  clearTimer(exitTimer);
  dismissTimer = null;
  exitTimer = null;
}

function beginExit(id: string, force = false) {
  const current = snapshot.current;
  if (!current || current.id !== id || current.leaving) return;

  const elapsed = Date.now() - shownAt;
  if (!force && elapsed < MIN_VISIBLE_MS) {
    clearTimer(dismissTimer);
    dismissTimer = window.setTimeout(() => beginExit(id, true), MIN_VISIBLE_MS - elapsed);
    return;
  }

  clearTimers();
  update({ ...current, leaving: true });
  exitTimer = window.setTimeout(() => {
    if (snapshot.current?.id === id) update(null);
    exitTimer = null;
  }, EXIT_MS);
}

function showMessage(content: unknown, options: MessageOptions = {}) {
  const normalizedContent = String(content ?? "").trim();
  if (!normalizedContent) return null;

  clearTimers();
  const duration = Number.isFinite(options.duration) ? Math.max(0, options.duration!) : 2400;
  const id = `message-${Date.now()}-${seed += 1}`;
  shownAt = Date.now();
  update({ id, content: normalizedContent, tone: options.tone ?? "info", leaving: false });

  if (duration > 0) {
    dismissTimer = window.setTimeout(() => beginExit(id), Math.max(duration, MIN_VISIBLE_MS));
  }
  return id;
}

export const message = showMessage as MessageApi;
message.remove = (id) => beginExit(id);
message.clear = () => {
  const id = snapshot.current?.id;
  if (id) beginExit(id, true);
};
message.error = (cause, options = {}) => showMessage(
  cause instanceof Error ? cause.message : String(cause),
  { tone: "error", duration: ERROR_DURATION_MS, ...options },
);

export function subscribeToMessages(listener: () => void) {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function getMessageSnapshot() {
  return snapshot;
}

export function useMessage() {
  return message;
}
