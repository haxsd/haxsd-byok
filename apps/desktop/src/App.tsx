import { lazy, Suspense } from "react";
import { HashRouter, Navigate, Route, Routes } from "react-router-dom";
import { TooltipProvider } from "./shared/ui/Tooltip";
import { AppErrorBoundary } from "./shell/AppErrorBoundary";
import { AppFrame } from "./shell/AppFrame";
import { AppLayout } from "./shell/AppLayout";
import { CallsPage } from "./features/calls/CallsPage";
import { ModelLibraryPage } from "./features/models/ModelLibraryPage";
import { HomePage } from "./features/home/HomePage";

const CallDetailsPage = lazy(() => import("./features/calls/CallDetailsPage").then((m) => ({ default: m.CallDetailsPage })));
const CursorSettingsPage = lazy(() => import("./features/models/CursorSettingsPage").then((m) => ({ default: m.CursorSettingsPage })));
const DevinSettingsPage = lazy(() => import("./features/devin/DevinSettingsPage").then((m) => ({ default: m.DevinSettingsPage })));
const CodexSettingsPage = lazy(() => import("./features/codex/CodexSettingsPage").then((m) => ({ default: m.CodexSettingsPage })));
const ClaudeSettingsPage = lazy(() => import("./features/claude/ClaudeSettingsPage").then((m) => ({ default: m.ClaudeSettingsPage })));
const PluginManagementPage = lazy(() => import("./features/plugins/PluginManagementPage").then((m) => ({ default: m.PluginManagementPage })));
const SettingsPage = lazy(() => import("./features/settings/SettingsPage").then((m) => ({ default: m.SettingsPage })));
const TutorialPage = lazy(() => import("./features/tutorial/TutorialPage").then((m) => ({ default: m.TutorialPage })));

export function App() {
  return (
    /* 兜底必须在最外层：外壳里的任何一处抛错（导航、命令面板、状态条）都不该
       让整个窗口变成空白，见 AppErrorBoundary 的说明。 */
    <AppErrorBoundary>
      <TooltipProvider>
        <HashRouter>
          <Suspense fallback={null}>
            <Routes>
              <Route path="calls/:callId" element={<CallDetailsPage />} />
              <Route element={<AppFrame />}>
                <Route element={<AppLayout />}>
                  <Route index element={<HomePage />} />
                  <Route path="calls" element={<CallsPage />} />
                  <Route path="models" element={<ModelLibraryPage />} />
                  <Route path="harness/cursor" element={<CursorSettingsPage />} />
                  <Route path="harness/devin" element={<DevinSettingsPage />} />
                  <Route path="harness/codex" element={<CodexSettingsPage />} />
                  <Route path="harness/claude" element={<ClaudeSettingsPage />} />
                  <Route path="plugins" element={<PluginManagementPage />} />
                  <Route path="settings" element={<SettingsPage />} />
                  <Route path="tutorial" element={<TutorialPage />} />
                </Route>
                <Route path="*" element={<Navigate to="/" replace />} />
              </Route>
            </Routes>
          </Suspense>
        </HashRouter>
      </TooltipProvider>
    </AppErrorBoundary>
  );
}
