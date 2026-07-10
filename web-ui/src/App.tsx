import { useState } from "react";
import { BrowserRouter, Routes, Route, useNavigate, useLocation } from "react-router-dom";
import AppLayout from "@cloudscape-design/components/app-layout";
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

function Shell() {
  const navigate = useNavigate();
  const location = useLocation();
  const { count: pendingCount, refresh: refreshPending } = usePendingCount();
  const [pendingVisible, setPendingVisible] = useState(false);

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
            text: pendingCount > 0 ? `Pending Changes (${pendingCount})` : "Pending Changes",
            iconName: "status-pending",
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
      <Shell />
    </BrowserRouter>
  );
}
