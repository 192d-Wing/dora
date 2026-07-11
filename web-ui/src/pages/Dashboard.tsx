import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import Container from "@cloudscape-design/components/container";
import Header from "@cloudscape-design/components/header";
import SpaceBetween from "@cloudscape-design/components/space-between";
import StatusIndicator from "@cloudscape-design/components/status-indicator";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Spinner from "@cloudscape-design/components/spinner";
import Alert from "@cloudscape-design/components/alert";
import PieChart from "@cloudscape-design/components/pie-chart";
import { api, MetricsSummary, ReadinessResponse, ServerInfo } from "../api";

function formatUptime(seconds: number): string {
  const d = Math.floor(seconds / 86400);
  const h = Math.floor((seconds % 86400) / 3600);
  const m = Math.floor((seconds % 3600) / 60);
  const parts: string[] = [];
  if (d > 0) parts.push(`${d}d`);
  if (h > 0) parts.push(`${h}h`);
  parts.push(`${m}m`);
  return parts.join(" ");
}

function StatCard({ title, value, description }: Readonly<{ title: string; value: React.ReactNode; description?: string }>) {
  return (
    <Container>
      <Box variant="awsui-key-label">{title}</Box>
      <Box variant="awsui-value-large">{value}</Box>
      {description && (
        <Box fontSize="body-s" color="text-status-inactive">{description}</Box>
      )}
    </Container>
  );
}

const LEASE_STATES = ["leased", "reserved", "probated", "released", "expired"] as const;
const STATE_COLORS: Record<string, string> = {
  leased: "#037f0c",
  reserved: "#0972d3",
  probated: "#8D6B09",
  released: "#656871",
  expired: "#d91515",
};

interface LeaseBreakdown {
  total: number;
  byState: Record<string, number>;
}

async function fetchLeaseBreakdown(family: "v4" | "v6"): Promise<LeaseBreakdown> {
  const fetcher = family === "v4" ? api.leasesV4 : api.leasesV6;
  const results = await Promise.all(
    LEASE_STATES.map((state) =>
      fetcher({ limit: "1", state }).then((res) => ({ state, count: res.meta.total }))
        .catch(() => ({ state, count: 0 }))
    )
  );
  const byState: Record<string, number> = {};
  let total = 0;
  for (const r of results) {
    byState[r.state] = r.count;
    total += r.count;
  }
  return { total, byState };
}

function LeaseBreakdownChart({ title, breakdown }: Readonly<{ title: string; breakdown: LeaseBreakdown }>) {
  const data = LEASE_STATES
    .filter((s) => breakdown.byState[s] > 0)
    .map((s) => ({
      title: s.charAt(0).toUpperCase() + s.slice(1),
      value: breakdown.byState[s],
      color: STATE_COLORS[s],
    }));

  if (breakdown.total === 0) {
    return (
      <Container header={<Header variant="h2">{title}</Header>}>
        <Box textAlign="center" padding="l" color="text-status-inactive">
          No leases
        </Box>
      </Container>
    );
  }

  return (
    <Container header={<Header variant="h2">{title}</Header>}>
      <PieChart
        data={data}
        size="medium"
        variant="donut"
        innerMetricValue={String(breakdown.total)}
        innerMetricDescription="total"
        hideFilter
        hideLegend={false}
        empty={<Box>No data</Box>}
        noMatch={<Box>No matching data</Box>}
      />
    </Container>
  );
}

function modeIndicator(mode: string): "success" | "warning" | "error" {
  if (mode === "normal") return "success";
  if (mode === "maintenance") return "warning";
  return "error";
}

export default function Dashboard() {
  const [server, setServer] = useState<ServerInfo | null>(null);
  const [readiness, setReadiness] = useState<ReadinessResponse | null>(null);
  const [metrics, setMetrics] = useState<MetricsSummary | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);

  const [v4Breakdown, setV4Breakdown] = useState<LeaseBreakdown | null>(null);
  const [v6Breakdown, setV6Breakdown] = useState<LeaseBreakdown | null>(null);
  const [v4ReservationCount, setV4ReservationCount] = useState<number | null>(null);
  const [v6ReservationCount, setV6ReservationCount] = useState<number | null>(null);
  const [pendingCount, setPendingCount] = useState<number | null>(null);

  const load = () => {
    setLoading(true);
    setError(null);
    Promise.all([
      api.server().catch(() => null),
      api.ready().catch(() => null),
      api.metricsSummary().catch(() => null),
      fetchLeaseBreakdown("v4").catch(() => null),
      fetchLeaseBreakdown("v6").catch(() => null),
      api.reservationsV4({ limit: "1" }).then((r) => r.meta.total).catch(() => null),
      api.reservationsV6({ limit: "1" }).then((r) => r.meta.total).catch(() => null),
      api.configCandidates({ limit: "100" })
        .then((r) => r.items.filter((c) =>
          c.status === "staged" || c.status === "valid" || c.status === "validating"
        ).length)
        .catch(() => null),
    ])
      .then(([srv, rdy, met, v4b, v6b, v4r, v6r, pend]) => {
        setServer(srv);
        setReadiness(rdy);
        setMetrics(met);
        setV4Breakdown(v4b);
        setV6Breakdown(v6b);
        setV4ReservationCount(v4r);
        setV6ReservationCount(v6r);
        setPendingCount(pend);
        if (!srv && !rdy && !met) {
          setError("Unable to reach the Dora API. Is the server running?");
        }
      })
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
    const interval = setInterval(load, 30000);
    return () => clearInterval(interval);
  }, []);

  if (loading && !server) {
    return <Spinner size="large" />;
  }

  const totalLeases = (v4Breakdown?.total ?? 0) + (v6Breakdown?.total ?? 0);
  const totalReservations = (v4ReservationCount ?? 0) + (v6ReservationCount ?? 0);

  return (
    <ContentLayout
      header={
        <Header
          variant="h1"
          description="DHCP server overview"
          actions={<Button iconName="refresh" onClick={load} />}
        >
          Dashboard
        </Header>
      }
    >
      <SpaceBetween size="l">
        {error && <Alert type="error">{error}</Alert>}

        {/* Server info bar */}
        {server && (
          <Container
            header={
              <Header
                variant="h2"
                description={`Started ${new Date(server.started_at).toLocaleString()}`}
              >
                {server.id}
              </Header>
            }
          >
            <ColumnLayout columns={4} variant="text-grid">
              <div>
                <Box variant="awsui-key-label">Version</Box>
                <Box fontSize="heading-s">{server.version}</Box>
              </div>
              <div>
                <Box variant="awsui-key-label">Mode</Box>
                <StatusIndicator
                  type={modeIndicator(server.mode)}
                >
                  {server.mode}
                </StatusIndicator>
              </div>
              <div>
                <Box variant="awsui-key-label">Uptime</Box>
                <Box fontSize="heading-s">{metrics ? formatUptime(metrics.uptime_seconds) : "-"}</Box>
              </div>
              <div>
                <Box variant="awsui-key-label">API Auth</Box>
                <Box fontSize="heading-s">{server.api.auth.join(", ") || "none"}</Box>
              </div>
            </ColumnLayout>
          </Container>
        )}

        {/* Quick stats */}
        <ColumnLayout columns={4}>
          <StatCard
            title="Active Leases"
            value={totalLeases.toLocaleString()}
            description={`v4: ${v4Breakdown?.total ?? 0} / v6: ${v6Breakdown?.total ?? 0}`}
          />
          <StatCard
            title="Reservations"
            value={totalReservations.toLocaleString()}
            description={`v4: ${v4ReservationCount ?? 0} / v6: ${v6ReservationCount ?? 0}`}
          />
          <StatCard
            title="In-Flight"
            value={metrics?.in_flight.toLocaleString() ?? "-"}
            description="Active API requests"
          />
          <StatCard
            title="Pending Changes"
            value={pendingCount ?? 0}
            description={pendingCount ? "Commit from top nav" : "No pending changes"}
          />
        </ColumnLayout>

        {/* Lease breakdown charts */}
        {(v4Breakdown || v6Breakdown) && (
          <ColumnLayout columns={2}>
            {v4Breakdown && (
              <LeaseBreakdownChart title="DHCPv4 Leases" breakdown={v4Breakdown} />
            )}
            {v6Breakdown && (
              <LeaseBreakdownChart title="DHCPv6 Leases" breakdown={v6Breakdown} />
            )}
          </ColumnLayout>
        )}

        {/* Protocol metrics */}
        {metrics && (
          <ColumnLayout columns={2}>
            <Container header={<Header variant="h2">DHCPv4 Traffic</Header>}>
              <ColumnLayout columns={3} variant="text-grid">
                <div>
                  <Box variant="awsui-key-label">Received</Box>
                  <Box variant="awsui-value-large">{metrics.dhcpv4.messages_received.toLocaleString()}</Box>
                </div>
                <div>
                  <Box variant="awsui-key-label">Sent</Box>
                  <Box variant="awsui-value-large">{metrics.dhcpv4.messages_sent.toLocaleString()}</Box>
                </div>
                <div>
                  <Box variant="awsui-key-label">Errors</Box>
                  <Box variant="awsui-value-large" color={metrics.dhcpv4.errors > 0 ? "text-status-error" : undefined}>
                    {metrics.dhcpv4.errors.toLocaleString()}
                  </Box>
                </div>
              </ColumnLayout>
            </Container>

            <Container header={<Header variant="h2">DHCPv6 Traffic</Header>}>
              <ColumnLayout columns={3} variant="text-grid">
                <div>
                  <Box variant="awsui-key-label">Received</Box>
                  <Box variant="awsui-value-large">{metrics.dhcpv6.messages_received.toLocaleString()}</Box>
                </div>
                <div>
                  <Box variant="awsui-key-label">Sent</Box>
                  <Box variant="awsui-value-large">{metrics.dhcpv6.messages_sent.toLocaleString()}</Box>
                </div>
                <div>
                  <Box variant="awsui-key-label">Errors</Box>
                  <Box variant="awsui-value-large" color={metrics.dhcpv6.errors > 0 ? "text-status-error" : undefined}>
                    {metrics.dhcpv6.errors.toLocaleString()}
                  </Box>
                </div>
              </ColumnLayout>
            </Container>
          </ColumnLayout>
        )}

        {/* Health checks */}
        {readiness && (
          <Container header={<Header variant="h2">Health Checks</Header>}>
            <ColumnLayout columns={Math.min(readiness.checks.length + 1, 4)} variant="text-grid">
              <div>
                <Box variant="awsui-key-label">Overall</Box>
                <StatusIndicator type={readiness.status === "ready" ? "success" : "error"}>
                  {readiness.status === "ready" ? "Ready" : "Not Ready"}
                </StatusIndicator>
              </div>
              {readiness.checks.map((check) => {
                let indicatorType: "success" | "warning" | "error" = "error";
                if (check.status === "pass") indicatorType = "success";
                else if (check.status === "warn") indicatorType = "warning";
                return (
                  <div key={check.name}>
                    <Box variant="awsui-key-label">
                      {check.name.charAt(0).toUpperCase() + check.name.slice(1)}
                    </Box>
                    <StatusIndicator type={indicatorType}>
                      {check.status}{check.message ? ` — ${check.message}` : ""}
                    </StatusIndicator>
                  </div>
                );
              })}
            </ColumnLayout>
          </Container>
        )}
      </SpaceBetween>
    </ContentLayout>
  );
}
