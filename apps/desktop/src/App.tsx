import { useEffect, useRef } from "react";
import { HashRouter, Navigate, Route, Routes } from "react-router-dom";
import { TooltipProvider } from "./shared/ui/Tooltip";
import { MessageProvider } from "./shared/ui/MessageProvider";
import { useMessage } from "./shared/ui/message";
import { AppFrame } from "./shell/AppFrame";
import { AppLayout } from "./shell/AppLayout";
import { CallsPage } from "./features/calls/CallsPage";
import { CallDetailsPage } from "./features/calls/CallDetailsPage";
import { CursorSettingsPage } from "./features/models/CursorSettingsPage";
import { ModelLibraryPage } from "./features/models/ModelLibraryPage";
import { DevinSettingsPage } from "./features/devin/DevinSettingsPage";
import { HomePage } from "./features/home/HomePage";
import { PluginManagementPage } from "./features/plugins/PluginManagementPage";
import { SettingsPage } from "./features/settings/SettingsPage";
import { TutorialPage } from "./features/tutorial/TutorialPage";
import { useAppStore } from "./shared/store/appStore";

export function App() {
  return (
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
              <Route path="plugins" element={<PluginManagementPage />} />
              <Route path="settings" element={<SettingsPage />} />
              <Route path="tutorial" element={<TutorialPage />} />
            </Route>
            <Route path="*" element={<Navigate to="/" replace />} />
          </Route>
        </Routes>
      </HashRouter>
      <AppMessages />
    </TooltipProvider>
  );
}

function AppMessages() {
  const { error } = useAppStore();
  const previousError = useRef<string | null>(null);
  const showMessage = useMessage();

  useEffect(() => {
    if (error && error !== previousError.current) showMessage(error, { tone: "error" });
    previousError.current = error;
  }, [error, showMessage]);

  return <MessageProvider />;
}
