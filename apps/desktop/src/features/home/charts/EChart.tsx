import { BarChart, GaugeChart, LineChart } from "echarts/charts";
import { GridComponent, LegendComponent, MarkLineComponent, TooltipComponent } from "echarts/components";
import { getInstanceByDom, init, use, type EChartsCoreOption } from "echarts/core";
import { CanvasRenderer } from "echarts/renderers";
import { useEffect, useRef, type MouseEventHandler } from "react";
import styles from "./EChart.module.scss";

use([BarChart, GaugeChart, LineChart, GridComponent, LegendComponent, MarkLineComponent, TooltipComponent, CanvasRenderer]);

type EChartProps = {
  option: EChartsCoreOption;
  className?: string;
  onMouseEnter?: MouseEventHandler<HTMLDivElement>;
  onMouseLeave?: MouseEventHandler<HTMLDivElement>;
  /** 图表内的点击，参数是 ECharts 的事件对象（含 dataIndex）。 */
  onClick?: (params: unknown) => void;
};

export function EChart({ option, className = styles.root, onMouseEnter, onMouseLeave, onClick }: EChartProps) {
  const element = useRef<HTMLDivElement>(null);
  const optionRef = useRef<EChartsCoreOption | null>(null);
  // The handler is registered against a ref, so a chart that gains an onClick later
  // does not need the instance to be recreated.
  const clickHandler = useRef(onClick);
  clickHandler.current = onClick;

  // 图表实例与 canvas 只在看得见的时候存在：路由是 keep-alive 的，离开首页后组件留在
  // DOM 里（display:none），ECharts 的画布却会一直占着（GPU 进程里这一坨最值钱）。
  // 视口观察器在被藏起来的那一刻给出 isIntersecting=false，据此销毁实例，回到页面再建。
  useEffect(() => {
    const node = element.current;
    if (!node) return;
    let chart: ReturnType<typeof init> | null = null;
    let resize: ResizeObserver | null = null;
    const handler = (params: unknown) => clickHandler.current?.(params);
    const show = () => {
      if (chart) return;
      chart = init(node, undefined, { renderer: "canvas" });
      chart.on("click", handler);
      if (optionRef.current) chart.setOption(optionRef.current, { notMerge: true });
      resize = new ResizeObserver(() => chart?.resize());
      resize.observe(node);
    };
    const hide = () => {
      resize?.disconnect();
      resize = null;
      chart?.off("click", handler);
      chart?.dispose();
      chart = null;
    };
    const visibility = new IntersectionObserver((entries) => {
      if (entries.some((entry) => entry.isIntersecting)) show();
      else hide();
    });
    visibility.observe(node);
    return () => {
      visibility.disconnect();
      hide();
    };
  }, []);

  useEffect(() => {
    optionRef.current = option;
    const node = element.current;
    if (node) getInstanceByDom(node)?.setOption(option, { notMerge: true });
  }, [option]);

  return <div ref={element} className={className} onMouseEnter={onMouseEnter} onMouseLeave={onMouseLeave} />;
}
