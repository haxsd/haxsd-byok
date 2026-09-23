import { Component, type ErrorInfo, type ReactNode } from "react";
import { EmptyState } from "../shared/ui/EmptyState";
import { alertCircleIcon } from "../shared/ui/icons";
import { Button } from "../shared/ui/Button";

type PageErrorBoundaryProps = {
  children: ReactNode;
  /** 出错的页面路径；路径变化时自动重试，否则一个坏页面会把整个外壳留在这里。 */
  resetKey: string;
};

type PageErrorBoundaryState = {
  error: Error | null;
};

/**
 * 页面级错误边界。
 *
 * 一次渲染异常原本会把整个 React 树卸载掉——外壳、导航、其他页面全部消失，只剩一块
 * 空白窗口。真实发生过一次（设置页里一个未定义的标识符），从外面看就是「应用白屏」，
 * 完全看不出是哪一页出了什么问题。这里把错误限制在内容区，并把原始信息显示出来。
 */
export class PageErrorBoundary extends Component<PageErrorBoundaryProps, PageErrorBoundaryState> {
  state: PageErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): PageErrorBoundaryState {
    return { error };
  }

  componentDidUpdate(previous: PageErrorBoundaryProps) {
    if (previous.resetKey !== this.props.resetKey && this.state.error) this.setState({ error: null });
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("page render failed", error, info.componentStack);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return <div style={{ padding: "var(--app-content-top) var(--app-page-padding) var(--app-page-padding)" }}>
      <EmptyState
        icon={alertCircleIcon}
        tone="bad"
        title={t("这个页面出错了")}
        description={this.state.error.message}
        actions={<Button onClick={() => this.setState({ error: null })}>{t("重新加载这个页面")}</Button>}
      />
    </div>;
  }
}
