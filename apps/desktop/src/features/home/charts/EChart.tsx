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

  useEffect(() => {
    const node = element.current;
    if (!node) return;
    const chart = init(node, undefined, { renderer: "canvas" });
    const observer = new ResizeObserver(() => chart.resize());
    observer.observe(node);
    return () => {
      observer.disconnect();
      chart.dispose();
    };
  }, []);

  useEffect(() => {
    const node = element.current;
    if (node) getInstanceByDom(node)?.setOption(option, { notMerge: true });
  }, [option]);

  // The handler is registered once against a ref, so a chart that gains an onClick
  // later does not need the instance to be recreated.
  const clickHandler = useRef(onClick);
  clickHandler.current = onClick;
  useEffect(() => {
    const node = element.current;
    const chart = node ? getInstanceByDom(node) : undefined;
    if (!chart) return;
    const handler = (params: unknown) => clickHandler.current?.(params);
    chart.on("click", handler);
    return () => { chart.off("click", handler); };
  }, []);

  return <div ref={element} className={className} onMouseEnter={onMouseEnter} onMouseLeave={onMouseLeave} />;
}
