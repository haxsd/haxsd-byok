import { Component, type ErrorInfo, type ReactNode } from "react";
import { EmptyState } from "../shared/ui/EmptyState";
import { alertCircleIcon } from "../shared/ui/icons";
import { Button } from "../shared/ui/Button";

type PageErrorBoundaryProps = {
  children: ReactNode;
  /**
   * 出错后自动重试的触发键：键变化时清掉错误、让 children 重新挂载。
   *
   * 只给**外壳那层**边界用（它套在 keep-alive 容器外面，切页时键真的会变）。缓存节点内部的
   * 边界不传这个键：节点内的缓存键恒定，自动重试既不可达、也不该做（见 `PageCacheErrorBoundary`）。
   */
  resetKey?: string;
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

/**
 * keep-alive 缓存节点**内部**的边界。
 *
 * 原来只有一层页面级边界，套在整个缓存容器外面：任何一页渲染出错，容器连同其它页面的
 * 缓存状态（草稿、滚动位置、已加载的数据）一起被卸掉。keepalive-for-react 的
 * `errorElement` 渲染在每个缓存节点内部，于是错误只重建出错的那一页。
 *
 * 复位**只支持手动**（"重新加载这个页面"按钮）：缓存节点内的缓存键就是路由路径，节点存活
 * 期间恒定，所以"切走再切回来自动重试"不可达——keep-alive 切回是恢复同一个节点，不会重新
 * 挂载，边界实例和错误状态都还在（曾经按 `_cacheKey` 做复位键，实际上永远不会触发）。
 *
 * 也不该改成失败即自动重试：渲染错误通常是确定性的，自动重试只会把同一段崩溃循环原样重放。
 * 页面真被缓存淘汰、或路由重建时，边界会跟着重新挂载，那时自然就是一次新的尝试。
 */
export function PageCacheErrorBoundary({ children }: { children: ReactNode }) {
  return <PageErrorBoundary>{children}</PageErrorBoundary>;
}
