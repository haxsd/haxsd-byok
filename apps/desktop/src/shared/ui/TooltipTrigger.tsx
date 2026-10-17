import { cloneElement, useCallback, useEffect, useId, useState, type FocusEventHandler, type PointerEventHandler, type ReactElement } from "react";
import { useTooltip, type TooltipAnchor } from "./Tooltip";


function anchorFor(element: HTMLElement): TooltipAnchor {
  return { contextElement: element, getBoundingClientRect: () => element.getBoundingClientRect() };
}

type TriggerProps = {
  "aria-describedby"?: string;
  onPointerMove?: PointerEventHandler<HTMLElement>;
  onPointerLeave?: PointerEventHandler<HTMLElement>;
  onPointerDown?: PointerEventHandler<HTMLElement>;
  onFocus?: FocusEventHandler<HTMLElement>;
  onBlur?: FocusEventHandler<HTMLElement>;
};

export function TooltipTrigger({ label, children }: { label: string; children: ReactElement<TriggerProps> }) {
  const { show: showTooltip, hide: hideTooltip } = useTooltip();
  const [anchor, setAnchor] = useState<TooltipAnchor | null>(null);
  const tooltipId = useId();

  const hide = useCallback(() => {
    setAnchor(null);
    hideTooltip();
  }, [hideTooltip]);

  useEffect(() => {
    if (!anchor) return;
    const close = (event: KeyboardEvent) => { if (event.key === "Escape") hide(); };
    document.addEventListener("keydown", close);
    return () => document.removeEventListener("keydown", close);
  }, [anchor, hide]);

  const trigger = cloneElement(children, {
    "aria-describedby": [children.props["aria-describedby"], anchor ? tooltipId : null].filter(Boolean).join(" ") || undefined,
    onPointerMove: (event) => {
      children.props.onPointerMove?.(event);
      if (event.pointerType !== "touch" && event.buttons === 0 && !anchor) {
        const nextAnchor = anchorFor(event.currentTarget);
        setAnchor(nextAnchor);
        showTooltip(nextAnchor, tooltipId, label);
      }
    },
    onPointerLeave: (event) => {
      children.props.onPointerLeave?.(event);
      hide();
    },
    onPointerDown: (event) => {
      children.props.onPointerDown?.(event);
      hide();
    },
    onFocus: (event) => {
      children.props.onFocus?.(event);
      if (event.currentTarget.matches(":focus-visible")) {
        const nextAnchor = anchorFor(event.currentTarget);
        setAnchor(nextAnchor);
        showTooltip(nextAnchor, tooltipId, label);
      } else {
        setAnchor(null);
      }
    },
    onBlur: (event) => {
      children.props.onBlur?.(event);
      hide();
    },
  });

  return trigger;
}

