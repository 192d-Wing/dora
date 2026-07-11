import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import Container from "@cloudscape-design/components/container";
import FormField from "@cloudscape-design/components/form-field";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Select from "@cloudscape-design/components/select";
import SpaceBetween from "@cloudscape-design/components/space-between";
import ContentLayout from "@cloudscape-design/components/content-layout";
import StatusIndicator from "@cloudscape-design/components/status-indicator";
import { applyMode, Mode } from "@cloudscape-design/global-styles";
import { api } from "../api";
import { useNotifications } from "../components/Notifications";

const THEME_OPTIONS = [
  { label: "Dark", value: "dark" },
  { label: "Light", value: "light" },
];

const REFRESH_OPTIONS = [
  { label: "Off", value: "0" },
  { label: "10 seconds", value: "10" },
  { label: "15 seconds", value: "15" },
  { label: "30 seconds", value: "30" },
  { label: "1 minute", value: "60" },
  { label: "5 minutes", value: "300" },
];

function ApiStatus({ reachable, version }: Readonly<{ reachable: boolean | null; version: string | null }>) {
  if (reachable === null) return <Box>Checking...</Box>;
  if (reachable) {
    return (
      <StatusIndicator type="success">
        Connected{version ? ` (API ${version})` : ""}
      </StatusIndicator>
    );
  }
  return <StatusIndicator type="error">Unreachable</StatusIndicator>;
}

export default function Settings() {
  const { notify } = useNotifications();
  const [token, setToken] = useState(localStorage.getItem("dora_api_token") ?? "");
  const savedTheme = localStorage.getItem("dora_theme") ?? "dark";
  const [theme, setTheme] = useState(THEME_OPTIONS.find((t) => t.value === savedTheme) ?? THEME_OPTIONS[0]);
  const savedRefresh = localStorage.getItem("dora_default_refresh") ?? "30";
  const [refreshInterval, setRefreshInterval] = useState(
    REFRESH_OPTIONS.find((r) => r.value === savedRefresh) ?? REFRESH_OPTIONS[3]
  );

  const [apiVersion, setApiVersion] = useState<string | null>(null);
  const [apiReachable, setApiReachable] = useState<boolean | null>(null);

  useEffect(() => {
    api.server()
      .then((s) => {
        setApiVersion(s.api.version);
        setApiReachable(true);
      })
      .catch(() => setApiReachable(false));
  }, []);

  const saveToken = () => {
    if (token) {
      localStorage.setItem("dora_api_token", token);
    } else {
      localStorage.removeItem("dora_api_token");
    }
    notify("success", "API token saved.");
  };

  const applyTheme = (option: typeof THEME_OPTIONS[0]) => {
    setTheme(option);
    localStorage.setItem("dora_theme", option.value);
    applyMode(option.value === "light" ? Mode.Light : Mode.Dark);
  };

  const saveRefreshInterval = (option: typeof REFRESH_OPTIONS[0]) => {
    setRefreshInterval(option);
    localStorage.setItem("dora_default_refresh", option.value);
  };

  return (
    <ContentLayout header={<Header variant="h1">Settings</Header>}>
      <SpaceBetween size="l">
        <Container header={<Header variant="h2">Appearance</Header>}>
          <ColumnLayout columns={2}>
            <FormField
              label="Theme"
              description="Switch between dark and light mode. Applied immediately."
            >
              <Select
                selectedOption={theme}
                onChange={({ detail }) =>
                  applyTheme(detail.selectedOption as typeof THEME_OPTIONS[0])
                }
                options={THEME_OPTIONS}
              />
            </FormField>
            <FormField
              label="Default Dashboard Refresh"
              description="Default auto-refresh interval for the Dashboard page."
            >
              <Select
                selectedOption={refreshInterval}
                onChange={({ detail }) =>
                  saveRefreshInterval(detail.selectedOption as typeof REFRESH_OPTIONS[0])
                }
                options={REFRESH_OPTIONS}
              />
            </FormField>
          </ColumnLayout>
        </Container>

        <Container header={<Header variant="h2">API Connection</Header>}>
          <SpaceBetween size="l">
            <ColumnLayout columns={2}>
              <FormField label="API Endpoint">
                <Box fontSize="heading-s" fontWeight="bold">
                  {window.location.origin}
                </Box>
              </FormField>
              <FormField label="Status">
                <ApiStatus reachable={apiReachable} version={apiVersion} />
              </FormField>
            </ColumnLayout>
            <FormField
              label="Bearer Token"
              description="If the Dora API requires authentication, enter the token here. Stored in localStorage."
            >
              <Input
                type="password"
                value={token}
                onChange={({ detail }) => setToken(detail.value)}
                placeholder="Enter API token"
              />
            </FormField>
            <Box>
              <Button variant="primary" onClick={saveToken}>
                Save token
              </Button>
            </Box>
          </SpaceBetween>
        </Container>

        <Container header={<Header variant="h2">Keyboard Shortcuts</Header>}>
          <ColumnLayout columns={2} variant="text-grid">
            <div>
              <Box variant="awsui-key-label">Navigation</Box>
              <Box fontSize="body-s">
                <strong>1</strong> Dashboard &middot;{" "}
                <strong>2</strong> Leases &middot;{" "}
                <strong>3</strong> Reservations &middot;{" "}
                <strong>4</strong> Pools &middot;{" "}
                <strong>5</strong> Config &middot;{" "}
                <strong>6</strong> Actions
              </Box>
            </div>
            <div>
              <Box variant="awsui-key-label">Actions</Box>
              <Box fontSize="body-s">
                <strong>C</strong> Open commit modal &middot;{" "}
                <strong>?</strong> Settings
              </Box>
            </div>
          </ColumnLayout>
        </Container>
      </SpaceBetween>
    </ContentLayout>
  );
}
