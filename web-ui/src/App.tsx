import { BrowserRouter, Routes, Route, useNavigate, useLocation } from "react-router-dom";
import AppLayout from "@cloudscape-design/components/app-layout";
import SideNavigation from "@cloudscape-design/components/side-navigation";
import TopNavigation from "@cloudscape-design/components/top-navigation";
import Dashboard from "./pages/Dashboard";
import Leases from "./pages/Leases";
import Settings from "./pages/Settings";

function Shell() {
  const navigate = useNavigate();
  const location = useLocation();

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
              { type: "divider" },
              { type: "link", text: "Settings", href: "/settings" },
            ]}
          />
        }
        content={
          <Routes>
            <Route path="/" element={<Dashboard />} />
            <Route path="/leases" element={<Leases />} />
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
