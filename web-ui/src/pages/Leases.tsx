import { useCallback, useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Pagination from "@cloudscape-design/components/pagination";
import Select from "@cloudscape-design/components/select";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Table from "@cloudscape-design/components/table";
import Tabs from "@cloudscape-design/components/tabs";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Alert from "@cloudscape-design/components/alert";
import Modal from "@cloudscape-design/components/modal";
import { api, V4Lease, V6Lease } from "../api";
import { useNotifications } from "../components/Notifications";
import RefreshControl, { useAutoRefresh } from "../components/RefreshControl";
import { toCsv, downloadCsv } from "../utils/csv";

const PAGE_SIZE = 50;

const STATE_OPTIONS = [
  { label: "All states", value: "" },
  { label: "Leased", value: "leased" },
  { label: "Reserved", value: "reserved" },
  { label: "Probated", value: "probated" },
  { label: "Released", value: "released" },
  { label: "Expired", value: "expired" },
];

function V4LeaseTable() {
  const { notify } = useNotifications();
  const [items, setItems] = useState<V4Lease[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [stateFilter, setStateFilter] = useState(STATE_OPTIONS[0]);
  const [ipFilter, setIpFilter] = useState("");
  const [networkFilter, setNetworkFilter] = useState("");
  const [clientIdFilter, setClientIdFilter] = useState("");
  const [releaseTarget, setReleaseTarget] = useState<V4Lease | null>(null);
  const [releasing, setReleasing] = useState(false);

  const confirmRelease = async () => {
    if (!releaseTarget) return;
    setReleasing(true);
    setError(null);
    try {
      await api.releaseLease("v4", releaseTarget.ip);
      notify("success", `Lease ${releaseTarget.ip} released.`);
      setReleaseTarget(null);
      load();
    } catch (err) {
      setError(String(err));
    } finally {
      setReleasing(false);
    }
  };

  const load = useCallback(() => {
    setLoading(true);
    setError(null);
    const params: Record<string, string> = {
      limit: String(PAGE_SIZE),
      offset: String((page - 1) * PAGE_SIZE),
    };
    if (stateFilter.value) params.state = stateFilter.value;
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;

    api
      .leasesV4(params)
      .then((res) => {
        setItems(res.items);
        setTotal(res.meta.total);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [page, stateFilter.value, ipFilter, networkFilter, clientIdFilter]);

  const v4Refresh = useAutoRefresh(load, 0);

  useEffect(() => {
    load();
  }, [load]);

  const exportCsv = async () => {
    const params: Record<string, string> = { limit: "1000", offset: "0" };
    if (stateFilter.value) params.state = stateFilter.value;
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;
    const res = await api.leasesV4(params);
    const headers = ["IP", "State", "Network", "Client ID", "Expires", "Source"];
    const rows = res.items.map((l) => [
      l.ip, l.state, l.network, l.client_id ?? "", l.expires_at ?? "", l.source ?? "",
    ]);
    downloadCsv("dhcpv4-leases.csv", toCsv(headers, rows));
  };

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
      <Table
        loading={loading}
        loadingText="Loading leases..."
        items={items}
        trackBy="ip"
        variant="full-page"
        stickyHeader
        header={
          <Header
            counter={`(${total})`}
            actions={
              <SpaceBetween direction="horizontal" size="xs">
                <Button iconName="download" onClick={exportCsv}>Export CSV</Button>
                <RefreshControl
                  onRefresh={load}
                  intervalSeconds={v4Refresh.intervalSeconds}
                  onIntervalChange={v4Refresh.setIntervalSeconds}
                  paused={v4Refresh.paused}
                  onPausedChange={v4Refresh.setPaused}
                />
              </SpaceBetween>
            }
          >
            DHCPv4 Leases
          </Header>
        }
        filter={
          <SpaceBetween direction="horizontal" size="s">
            <Select
              selectedOption={stateFilter}
              onChange={({ detail }) => {
                setStateFilter(detail.selectedOption as typeof stateFilter);
                setPage(1);
              }}
              options={STATE_OPTIONS}
            />
            <Input
              placeholder="Filter by IP"
              value={ipFilter}
              onChange={({ detail }) => setIpFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") {
                  setPage(1);
                  load();
                }
              }}
            />
            <Input
              placeholder="Filter by network (CIDR)"
              value={networkFilter}
              onChange={({ detail }) => setNetworkFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") {
                  setPage(1);
                  load();
                }
              }}
            />
            <Input
              placeholder="Filter by client ID"
              value={clientIdFilter}
              onChange={({ detail }) => setClientIdFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") {
                  setPage(1);
                  load();
                }
              }}
            />
            <Button onClick={() => { setPage(1); load(); }}>Search</Button>
          </SpaceBetween>
        }
        pagination={
          <Pagination
            currentPageIndex={page}
            pagesCount={Math.max(1, Math.ceil(total / PAGE_SIZE))}
            onChange={({ detail }) => setPage(detail.currentPageIndex)}
          />
        }
        columnDefinitions={[
          {
            id: "ip",
            header: "IP Address",
            cell: (item) => item.ip,
            sortingField: "ip",
            width: 180,
          },
          {
            id: "state",
            header: "State",
            cell: (item) => <LeaseStateBadge state={item.state} />,
            sortingField: "state",
            width: 120,
          },
          {
            id: "network",
            header: "Network",
            cell: (item) => item.network,
            width: 180,
          },
          {
            id: "client_id",
            header: "Client ID",
            cell: (item) => item.client_id ?? "-",
            width: 220,
          },
          {
            id: "expires_at",
            header: "Expires",
            cell: (item) =>
              item.expires_at ? new Date(item.expires_at).toLocaleString() : "-",
            sortingField: "expires_at",
          },
          {
            id: "source",
            header: "Source",
            cell: (item) => item.source ?? "-",
            width: 100,
          },
          {
            id: "actions",
            header: "Actions",
            cell: (item) =>
              item.state === "leased" || item.state === "reserved" ? (
                <Button variant="inline-link" onClick={() => setReleaseTarget(item)}>
                  Release
                </Button>
              ) : (
                "-"
              ),
            width: 100,
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No leases</b>
            <Box variant="p" color="inherit">No DHCPv4 leases found.</Box>
          </Box>
        }
      />
      <Modal
        visible={releaseTarget !== null}
        onDismiss={() => setReleaseTarget(null)}
        header="Release Lease"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setReleaseTarget(null)}>Cancel</Button>
              <Button variant="primary" loading={releasing} onClick={confirmRelease}>Release</Button>
            </SpaceBetween>
          </Box>
        }
      >
        Release lease for <strong>{releaseTarget?.ip}</strong> in network{" "}
        <strong>{releaseTarget?.network}</strong>? The address will be returned to the pool.
      </Modal>
    </SpaceBetween>
  );
}

function V6LeaseTable() {
  const { notify } = useNotifications();
  const [items, setItems] = useState<V6Lease[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [stateFilter, setStateFilter] = useState(STATE_OPTIONS[0]);
  const [ipFilter, setIpFilter] = useState("");
  const [networkFilter, setNetworkFilter] = useState("");
  const [clientIdFilter, setClientIdFilter] = useState("");
  const [releaseTarget, setReleaseTarget] = useState<V6Lease | null>(null);
  const [releasing, setReleasing] = useState(false);

  const confirmRelease = async () => {
    if (!releaseTarget?.ip) return;
    setReleasing(true);
    setError(null);
    try {
      await api.releaseLease("v6", releaseTarget.ip);
      notify("success", `Lease ${releaseTarget.ip} released.`);
      setReleaseTarget(null);
      load();
    } catch (err) {
      setError(String(err));
    } finally {
      setReleasing(false);
    }
  };

  const load = useCallback(() => {
    setLoading(true);
    setError(null);
    const params: Record<string, string> = {
      limit: String(PAGE_SIZE),
      offset: String((page - 1) * PAGE_SIZE),
    };
    if (stateFilter.value) params.state = stateFilter.value;
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;

    api
      .leasesV6(params)
      .then((res) => {
        setItems(res.items);
        setTotal(res.meta.total);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [page, stateFilter.value, ipFilter, networkFilter, clientIdFilter]);

  const v6Refresh = useAutoRefresh(load, 0);

  useEffect(() => {
    load();
  }, [load]);

  const exportCsv = async () => {
    const params: Record<string, string> = { limit: "1000", offset: "0" };
    if (stateFilter.value) params.state = stateFilter.value;
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;
    const res = await api.leasesV6(params);
    const headers = ["IP", "Prefix", "State", "Type", "Network", "Client ID", "IAID", "Expires", "Source"];
    const rows = res.items.map((l) => [
      l.ip ?? "", l.prefix ?? "", l.state, l.lease_type, l.network,
      l.client_id ?? "", l.iaid != null ? String(l.iaid) : "", l.expires_at ?? "", l.source ?? "",
    ]);
    downloadCsv("dhcpv6-leases.csv", toCsv(headers, rows));
  };

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
      <Table
        loading={loading}
        loadingText="Loading leases..."
        items={items}
        trackBy={(item) => item.ip ?? item.prefix ?? ""}
        variant="full-page"
        stickyHeader
        header={
          <Header
            counter={`(${total})`}
            actions={
              <SpaceBetween direction="horizontal" size="xs">
                <Button iconName="download" onClick={exportCsv}>Export CSV</Button>
                <RefreshControl
                  onRefresh={load}
                  intervalSeconds={v6Refresh.intervalSeconds}
                  onIntervalChange={v6Refresh.setIntervalSeconds}
                  paused={v6Refresh.paused}
                  onPausedChange={v6Refresh.setPaused}
                />
              </SpaceBetween>
            }
          >
            DHCPv6 Leases
          </Header>
        }
        filter={
          <SpaceBetween direction="horizontal" size="s">
            <Select
              selectedOption={stateFilter}
              onChange={({ detail }) => {
                setStateFilter(detail.selectedOption as typeof stateFilter);
                setPage(1);
              }}
              options={STATE_OPTIONS}
            />
            <Input
              placeholder="Filter by IP / prefix"
              value={ipFilter}
              onChange={({ detail }) => setIpFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") {
                  setPage(1);
                  load();
                }
              }}
            />
            <Input
              placeholder="Filter by network (CIDR)"
              value={networkFilter}
              onChange={({ detail }) => setNetworkFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") {
                  setPage(1);
                  load();
                }
              }}
            />
            <Input
              placeholder="Filter by client ID (DUID)"
              value={clientIdFilter}
              onChange={({ detail }) => setClientIdFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") {
                  setPage(1);
                  load();
                }
              }}
            />
            <Button onClick={() => { setPage(1); load(); }}>Search</Button>
          </SpaceBetween>
        }
        pagination={
          <Pagination
            currentPageIndex={page}
            pagesCount={Math.max(1, Math.ceil(total / PAGE_SIZE))}
            onChange={({ detail }) => setPage(detail.currentPageIndex)}
          />
        }
        columnDefinitions={[
          {
            id: "ip",
            header: "IP / Prefix",
            cell: (item) => item.ip ?? item.prefix ?? "-",
            width: 280,
          },
          {
            id: "state",
            header: "State",
            cell: (item) => <LeaseStateBadge state={item.state} />,
            width: 120,
          },
          {
            id: "lease_type",
            header: "Type",
            cell: (item) => item.lease_type.toUpperCase(),
            width: 100,
          },
          {
            id: "network",
            header: "Network",
            cell: (item) => item.network,
            width: 250,
          },
          {
            id: "client_id",
            header: "Client ID",
            cell: (item) => item.client_id ?? "-",
            width: 220,
          },
          {
            id: "expires_at",
            header: "Expires",
            cell: (item) =>
              item.expires_at ? new Date(item.expires_at).toLocaleString() : "-",
          },
          {
            id: "source",
            header: "Source",
            cell: (item) => item.source ?? "-",
            width: 100,
          },
          {
            id: "actions",
            header: "Actions",
            cell: (item) =>
              (item.state === "leased" || item.state === "reserved") && item.ip ? (
                <Button variant="inline-link" onClick={() => setReleaseTarget(item)}>
                  Release
                </Button>
              ) : (
                "-"
              ),
            width: 100,
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No leases</b>
            <Box variant="p" color="inherit">No DHCPv6 leases found.</Box>
          </Box>
        }
      />
      <Modal
        visible={releaseTarget !== null}
        onDismiss={() => setReleaseTarget(null)}
        header="Release Lease"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setReleaseTarget(null)}>Cancel</Button>
              <Button variant="primary" loading={releasing} onClick={confirmRelease}>Release</Button>
            </SpaceBetween>
          </Box>
        }
      >
        Release lease for <strong>{releaseTarget?.ip}</strong> in network{" "}
        <strong>{releaseTarget?.network}</strong>? The address will be returned to the pool.
      </Modal>
    </SpaceBetween>
  );
}

function LeaseStateBadge({ state }: { state: string }) {
  const colors: Record<string, string> = {
    leased: "#037f0c",
    reserved: "#0972d3",
    probated: "#8D6B09",
    released: "#656871",
    expired: "#d91515",
  };
  return (
    <Box
      display="inline-block"
      fontSize="body-s"
      fontWeight="bold"
      color="inherit"
    >
      <span style={{ color: colors[state] ?? "#656871" }}>{state}</span>
    </Box>
  );
}

export default function Leases() {
  return (
    <ContentLayout header={<Header variant="h1">Leases</Header>}>
      <Tabs
        tabs={[
          { id: "v4", label: "DHCPv4", content: <V4LeaseTable /> },
          { id: "v6", label: "DHCPv6", content: <V6LeaseTable /> },
        ]}
      />
    </ContentLayout>
  );
}
