import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import Container from "@cloudscape-design/components/container";
import Header from "@cloudscape-design/components/header";
import SpaceBetween from "@cloudscape-design/components/space-between";
import StatusIndicator from "@cloudscape-design/components/status-indicator";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Spinner from "@cloudscape-design/components/spinner";
import Alert from "@cloudscape-design/components/alert";
import { api, MetricsSummary, ReadinessResponse, ServerInfo } from "../api";

function formatUptime(seconds: number): string {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const s = seconds % 60;
  const parts: string[] = [];
  if (d > 0) parts.push(`${d}d`);
  if (h > 0) parts.push(`${h}h`);
  if (m > 0) parts.push(`${m}m`);
  parts.push(`${s}s`);
  return parts.join(" ");
}

function ValueCard({
  title,
  value,
  compact,
}: {
  title: string;
  value: React.ReactNode;
  compact?: boolean;
}) {
  return (
    <div>
      <Box variant="awsui-key-label">{title}</Box>
      {compact ? (
        <Box fontSize="heading-s">{value}</Box>
      ) : (
        <Box variant="awsui-value-large">{value}</Box>
      )}
    </div>
  );
}

export default function Dashboard() {
  const [server, setServer] = useState<ServerInfo | null>(null);
  const [readiness, setReadiness] = useState<ReadinessResponse | null>(null);
  const [metrics, setMetrics] = useState<MetricsSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const load = () => {
    setLoading(true);
    setError(null);
    Promise.all([
      api.server().catch(() => null),
      api.ready().catch(() => null),
      api.metricsSummary().catch(() => null),
    ])
      .then(([srv, rdy, met]) => {
        setServer(srv);
        setReadiness(rdy);
        setMetrics(met);
        if (!srv && !rdy && !met) {
          setError("Unable to reach the Dora API. Is the server running?");
        }
      })
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
    const interval = setInterval(load, 15000);
    return () => clearInterval(interval);
  }, []);

  if (loading && !server) {
    return <Spinner size="large" />;
  }

  return (
    <ContentLayout
      header={
        <Header variant="h1" description="DHCP server overview">
          Dashboard
        </Header>
      }
    >
      <SpaceBetween size="l">
        {error && <Alert type="error">{error}</Alert>}

        {server && (
          <Container header={<Header variant="h2">Server</Header>}>
            <ColumnLayout columns={4} variant="text-grid">
              <ValueCard title="Server ID" value={server.id} compact />
              <ValueCard title="Version" value={server.version} compact />
              <ValueCard
                title="Mode"
                compact
                value={
                  <StatusIndicator
                    type={
                      server.mode === "normal"
                        ? "success"
                        : server.mode === "maintenance"
                          ? "warning"
                          : "error"
                    }
                  >
                    {server.mode}
                  </StatusIndicator>
                }
              />
              <ValueCard
                title="Started"
                value={new Date(server.started_at).toLocaleString()}
                compact
              />
            </ColumnLayout>
          </Container>
        )}

        {metrics && (
          <>
            <Container header={<Header variant="h2">Overview</Header>}>
              <ColumnLayout columns={3} variant="text-grid">
                <ValueCard title="Uptime" value={formatUptime(metrics.uptime_seconds)} compact />
                <ValueCard title="In-Flight Requests" value={metrics.in_flight.toLocaleString()} compact />
                <ValueCard title="Health" value={readiness?.status === "ready" ? "Healthy" : "Degraded"} compact />
              </ColumnLayout>
            </Container>

            <ColumnLayout columns={2}>
              <Container header={<Header variant="h2">DHCPv4</Header>}>
                <ColumnLayout columns={3} variant="text-grid">
                  <ValueCard
                    title="Messages Received"
                    value={metrics.dhcpv4.messages_received.toLocaleString()}
                    compact
                  />
                  <ValueCard
                    title="Messages Sent"
                    value={metrics.dhcpv4.messages_sent.toLocaleString()}
                    compact
                  />
                  <ValueCard
                    title="Errors"
                    value={metrics.dhcpv4.errors.toLocaleString()}
                    compact
                  />
                </ColumnLayout>
              </Container>

              <Container header={<Header variant="h2">DHCPv6</Header>}>
                <ColumnLayout columns={3} variant="text-grid">
                  <ValueCard
                    title="Messages Received"
                    value={metrics.dhcpv6.messages_received.toLocaleString()}
                    compact
                  />
                  <ValueCard
                    title="Messages Sent"
                    value={metrics.dhcpv6.messages_sent.toLocaleString()}
                    compact
                  />
                  <ValueCard
                    title="Errors"
                    value={metrics.dhcpv6.errors.toLocaleString()}
                    compact
                  />
                </ColumnLayout>
              </Container>
            </ColumnLayout>
          </>
        )}

        {readiness && (
          <Container header={<Header variant="h2">Health</Header>}>
            <ColumnLayout columns={readiness.checks.length + 1} variant="text-grid">
              <ValueCard
                title="Status"
                compact
                value={
                  <StatusIndicator
                    type={readiness.status === "ready" ? "success" : "error"}
                  >
                    {readiness.status === "ready" ? "Ready" : "Not Ready"}
                  </StatusIndicator>
                }
              />
              {readiness.checks.map((check) => (
                <ValueCard
                  key={check.name}
                  title={check.name.charAt(0).toUpperCase() + check.name.slice(1)}
                  compact
                  value={
                    <StatusIndicator
                      type={
                        check.status === "pass"
                          ? "success"
                          : check.status === "warn"
                            ? "warning"
                            : "error"
                      }
                    >
                      {check.status}{check.message ? ` — ${check.message}` : ""}
                    </StatusIndicator>
                  }
                />
              ))}
            </ColumnLayout>
          </Container>
        )}
      </SpaceBetween>
    </ContentLayout>
  );
}
