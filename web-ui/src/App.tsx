import { useMemo, useState } from "react";
import { BrowserRouter, Routes, Route, useNavigate, useLocation } from "react-router-dom";
import AppLayout from "@cloudscape-design/components/app-layout";
import BreadcrumbGroup from "@cloudscape-design/components/breadcrumb-group";
import SideNavigation from "@cloudscape-design/components/side-navigation";
import TopNavigation from "@cloudscape-design/components/top-navigation";
import Dashboard from "./pages/Dashboard";
import Leases from "./pages/Leases";
import Reservations from "./pages/Reservations";
import Config from "./pages/Config";
import Pools from "./pages/Pools";
import Actions from "./pages/Actions";
import Settings from "./pages/Settings";
import PendingChanges, { usePendingCount } from "./components/PendingChanges";
import { NotificationsProvider } from "./components/Notifications";
import { useKeyboardShortcuts } from "./hooks/useKeyboardShortcuts";

function Shell() {
  const navigate = useNavigate();
  const location = useLocation();
  const { count: pendingCount, refresh: refreshPending } = usePendingCount();
  const [pendingVisible, setPendingVisible] = useState(false);

  const shortcuts = useMemo(() => ({
    "c": () => setPendingVisible(true),
    "1": () => navigate("/"),
    "2": () => navigate("/leases"),
    "3": () => navigate("/reservations"),
    "4": () => navigate("/pools"),
    "5": () => navigate("/config"),
    "6": () => navigate("/actions"),
    "?": () => navigate("/settings"),
  }), [navigate]);

  useKeyboardShortcuts(shortcuts);

  const PAGE_LABELS: Record<string, string> = {
    "/": "Dashboard",
    "/leases": "Leases",
    "/reservations": "Reservations",
    "/pools": "Pools",
    "/config": "Configuration",
    "/actions": "Actions",
    "/settings": "Settings",
  };

  const breadcrumbs = [
    { text: "Dora", href: "/" },
    ...(location.pathname !== "/"
      ? [{ text: PAGE_LABELS[location.pathname] ?? location.pathname, href: location.pathname }]
      : []),
  ];

  return (
    <>
      <TopNavigation
        identity={{
          href: "/",
          title: "Dora DHCP",
        }}
        utilities={[
          {
            type: "button",
            text: pendingCount > 0 ? `Commit (${pendingCount})` : "Commit",
            iconName: "upload",
            badge: pendingCount > 0,
            onClick: () => setPendingVisible(true),
          },
          {
            type: "button",
            text: "API Docs",
            href: "/docs",
            external: true,
            externalIconAriaLabel: "(opens Swagger UI)",
          },
          {
            type: "button",
            iconName: "settings",
            title: "Settings",
            onClick: () => navigate("/settings"),
          },
        ]}
      />
      <PendingChanges
        visible={pendingVisible}
        onDismiss={() => setPendingVisible(false)}
        onActivated={refreshPending}
      />
      <AppLayout
        toolsHide
        breadcrumbs={
          <BreadcrumbGroup
            items={breadcrumbs}
            onFollow={(e) => {
              e.preventDefault();
              navigate(e.detail.href);
            }}
          />
        }
        navigation={
          <SideNavigation
            activeHref={location.pathname}
            onFollow={(e) => {
              e.preventDefault();
              navigate(e.detail.href);
            }}
            header={{ text: "Dora", href: "/" }}
            items={[
              { type: "link", text: "Dashboard", href: "/" },
              { type: "link", text: "Leases", href: "/leases" },
              { type: "link", text: "Reservations", href: "/reservations" },
              { type: "link", text: "Pools", href: "/pools" },
              { type: "link", text: "Configuration", href: "/config" },
              { type: "link", text: "Actions", href: "/actions" },
              { type: "divider" },
              { type: "link", text: "Settings", href: "/settings" },
            ]}
          />
        }
        content={
          <Routes>
            <Route path="/" element={<Dashboard />} />
            <Route path="/leases" element={<Leases />} />
            <Route path="/reservations" element={<Reservations />} />
            <Route path="/pools" element={<Pools />} />
            <Route path="/config" element={<Config />} />
            <Route path="/actions" element={<Actions />} />
            <Route path="/settings" element={<Settings />} />
          </Routes>
        }
      />
    </>
  );
}

export default function App() {
  return (
    <BrowserRouter>
      <NotificationsProvider>
        <Shell />
      </NotificationsProvider>
    </BrowserRouter>
  );
}
