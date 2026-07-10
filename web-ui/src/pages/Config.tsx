import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Container from "@cloudscape-design/components/container";
import Header from "@cloudscape-design/components/header";
import SpaceBetween from "@cloudscape-design/components/space-between";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Spinner from "@cloudscape-design/components/spinner";
import Alert from "@cloudscape-design/components/alert";
import StatusIndicator from "@cloudscape-design/components/status-indicator";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import { api, ConfigDocument } from "../api";

export default function Config() {
  const [config, setConfig] = useState<ConfigDocument | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  const load = () => {
    setLoading(true);
    setError(null);
    api
      .config()
      .then(setConfig)
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
  }, []);

  if (loading && !config) {
    return <Spinner size="large" />;
  }

  return (
    <ContentLayout
      header={
        <Header
          variant="h1"
          description="Active server configuration (secrets redacted)"
          actions={<Button iconName="refresh" onClick={load} />}
        >
          Configuration
        </Header>
      }
    >
      <SpaceBetween size="l">
        {error && <Alert type="error">{error}</Alert>}

        {config && (
          <>
            <Container header={<Header variant="h2">Metadata</Header>}>
              <ColumnLayout columns={2} variant="text-grid">
                <div>
                  <Box variant="awsui-key-label">Config Version</Box>
                  <Box fontSize="heading-s">{config.version}</Box>
                </div>
                <div>
                  <Box variant="awsui-key-label">Redacted</Box>
                  <StatusIndicator type={config.redacted ? "info" : "warning"}>
                    {config.redacted ? "Yes" : "No"}
                  </StatusIndicator>
                </div>
              </ColumnLayout>
            </Container>

            <Container header={<Header variant="h2">Document</Header>}>
              <Box>
                <pre
                  style={{
                    margin: 0,
                    padding: "12px",
                    borderRadius: "8px",
                    backgroundColor: "var(--color-background-container-content, #0f1b2d)",
                    color: "var(--color-text-body-default, #d1d5db)",
                    fontSize: "13px",
                    lineHeight: 1.5,
                    overflowX: "auto",
                    whiteSpace: "pre-wrap",
                    wordBreak: "break-word",
                  }}
                >
                  {JSON.stringify(config.document, null, 2)}
                </pre>
              </Box>
            </Container>
          </>
        )}
      </SpaceBetween>
    </ContentLayout>
  );
}
