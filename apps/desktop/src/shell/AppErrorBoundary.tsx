import { Component, type ErrorInfo, type ReactNode } from "react";
import { Button } from "../shared/ui/Button";
import { EmptyState } from "../shared/ui/EmptyState";
import { Icon } from "../shared/ui/Icon";
import { alertCircleIcon, refreshIcon } from "../shared/ui/icons";
import styles from "./AppErrorBoundary.module.scss";

type AppErrorBoundaryProps = {
  children: ReactNode;
};

type AppErrorBoundaryState = {
  error: Error | null;
};

/**
 * 整个界面的最后一道兜底。
 *
 * 页面级错误边界（PageErrorBoundary）只包住内容区，外壳本身——导航、命令面板、
 * 状态条、调用详情窗口——一直在边界之外。真实发生过：服务端返回的模型列表里带一个
 * null 时，命令面板在渲染里抛错，React 把**整棵树**卸掉，窗口只剩一片空白——没有
 * 导航、没有报错、没有任何恢复入口，只能手动关掉重开（用无头 Chrome 实测过：
 * root 子节点数为 0、导航与页头全部消失）。
 *
 * 这里让这种失败至少是可读、可恢复的：原始错误留在界面上（转发给开发者时就是这一行），
 * 并给「重试」和「重新加载这个页面」两个动作。完整堆栈写进控制台。
 */
export class AppErrorBoundary extends Component<AppErrorBoundaryProps, AppErrorBoundaryState> {
  state: AppErrorBoundaryState = { error: null };

  static getDerivedStateFromError(error: Error): AppErrorBoundaryState {
    return { error };
  }

  componentDidCatch(error: Error, info: ErrorInfo) {
    console.error("application render failed", error, info.componentStack);
  }

  render() {
    if (!this.state.error) return this.props.children;
    return <AppCrashFallback
      error={this.state.error}
      onRetry={() => this.setState({ error: null })}
    />;
  }
}

/**
 * 出错时整窗显示的内容。单独一个组件是为了能在测试里直接渲染它：SSR 不支持错误
 * 边界，兜底界面本身必须能被断言。
 *
 * 文案沿用页面级边界已有的两句话：同一件事（"这一屏坏了"）在整窗和单页上应该是
 * 同一套说法。
 */
export function AppCrashFallback({ error, onRetry }: { error: Error; onRetry: () => void }) {
  return <div className={styles.root}>
    <EmptyState
      icon={alertCircleIcon}
      tone="bad"
      title={t("这个页面出错了")}
      description={t("错误原文：{detail}。请把这条信息发给开发者。", { detail: error.message })}
      actions={<>
        <Button size="small" onClick={onRetry}>
          <Icon icon={refreshIcon} size="1em" />{t("重试")}
        </Button>
        <Button size="small" variant="primary" onClick={() => window.location.reload()}>{t("重新加载这个页面")}</Button>
      </>}
      compact
    />
  </div>;
}
