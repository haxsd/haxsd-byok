import { HashRouter, Navigate, Route, Routes } from "react-router-dom";
import { TooltipProvider } from "./shared/ui/Tooltip";
import { AppErrorBoundary } from "./shell/AppErrorBoundary";
import { AppFrame } from "./shell/AppFrame";
import { AppLayout } from "./shell/AppLayout";
import { CallsPage } from "./features/calls/CallsPage";
import { CallDetailsPage } from "./features/calls/CallDetailsPage";
import { CursorSettingsPage } from "./features/models/CursorSettingsPage";
import { ModelLibraryPage } from "./features/models/ModelLibraryPage";
import { DevinSettingsPage } from "./features/devin/DevinSettingsPage";
import { CodexSettingsPage } from "./features/codex/CodexSettingsPage";
import { HomePage } from "./features/home/HomePage";
import { PluginManagementPage } from "./features/plugins/PluginManagementPage";
import { SettingsPage } from "./features/settings/SettingsPage";
import { TutorialPage } from "./features/tutorial/TutorialPage";

export function App() {
  return (
    /* 兜底必须在最外层：外壳里的任何一处抛错（导航、命令面板、状态条）都不该
       让整个窗口变成空白，见 AppErrorBoundary 的说明。 */
    <AppErrorBoundary>
      <TooltipProvider>
        <HashRouter>
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
                <Route path="plugins" element={<PluginManagementPage />} />
                <Route path="settings" element={<SettingsPage />} />
                <Route path="tutorial" element={<TutorialPage />} />
              </Route>
              <Route path="*" element={<Navigate to="/" replace />} />
            </Route>
          </Routes>
        </HashRouter>
      </TooltipProvider>
    </AppErrorBoundary>
  );
}
