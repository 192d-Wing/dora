import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Pagination from "@cloudscape-design/components/pagination";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Table from "@cloudscape-design/components/table";
import Tabs from "@cloudscape-design/components/tabs";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Alert from "@cloudscape-design/components/alert";
import { api, V4Reservation, V6Reservation } from "../api";

const PAGE_SIZE = 50;

function V4ReservationTable() {
  const [items, setItems] = useState<V4Reservation[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [ipFilter, setIpFilter] = useState("");
  const [networkFilter, setNetworkFilter] = useState("");
  const [clientIdFilter, setClientIdFilter] = useState("");

  const load = () => {
    setLoading(true);
    setError(null);
    const params: Record<string, string> = {
      limit: String(PAGE_SIZE),
      offset: String((page - 1) * PAGE_SIZE),
    };
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;

    api
      .reservationsV4(params)
      .then((res) => {
        setItems(res.items);
        setTotal(res.meta.total);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
  }, [page]);

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error">{error}</Alert>}
      <Table
        loading={loading}
        loadingText="Loading reservations..."
        items={items}
        trackBy="ip"
        variant="full-page"
        stickyHeader
        header={
          <Header
            counter={`(${total})`}
            actions={<Button iconName="refresh" onClick={load} />}
          >
            DHCPv4 Reservations
          </Header>
        }
        filter={
          <SpaceBetween direction="horizontal" size="s">
            <Input
              placeholder="Filter by IP"
              value={ipFilter}
              onChange={({ detail }) => setIpFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") { setPage(1); load(); }
              }}
            />
            <Input
              placeholder="Filter by network (CIDR)"
              value={networkFilter}
              onChange={({ detail }) => setNetworkFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") { setPage(1); load(); }
              }}
            />
            <Input
              placeholder="Filter by client ID"
              value={clientIdFilter}
              onChange={({ detail }) => setClientIdFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") { setPage(1); load(); }
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
            width: 180,
          },
          {
            id: "network",
            header: "Network",
            cell: (item) => item.network ?? "-",
            width: 180,
          },
          {
            id: "source",
            header: "Source",
            cell: (item) => <SourceBadge source={item.source} />,
            width: 120,
          },
          {
            id: "match",
            header: "Match",
            cell: (item) => formatMatch(item.match),
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No reservations</b>
            <Box variant="p" color="inherit">No DHCPv4 reservations found.</Box>
          </Box>
        }
      />
    </SpaceBetween>
  );
}

function V6ReservationTable() {
  const [items, setItems] = useState<V6Reservation[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [ipFilter, setIpFilter] = useState("");
  const [networkFilter, setNetworkFilter] = useState("");
  const [clientIdFilter, setClientIdFilter] = useState("");

  const load = () => {
    setLoading(true);
    setError(null);
    const params: Record<string, string> = {
      limit: String(PAGE_SIZE),
      offset: String((page - 1) * PAGE_SIZE),
    };
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;

    api
      .reservationsV6(params)
      .then((res) => {
        setItems(res.items);
        setTotal(res.meta.total);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
  }, [page]);

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error">{error}</Alert>}
      <Table
        loading={loading}
        loadingText="Loading reservations..."
        items={items}
        trackBy={(item) => item.ip ?? item.prefix ?? ""}
        variant="full-page"
        stickyHeader
        header={
          <Header
            counter={`(${total})`}
            actions={<Button iconName="refresh" onClick={load} />}
          >
            DHCPv6 Reservations
          </Header>
        }
        filter={
          <SpaceBetween direction="horizontal" size="s">
            <Input
              placeholder="Filter by IP"
              value={ipFilter}
              onChange={({ detail }) => setIpFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") { setPage(1); load(); }
              }}
            />
            <Input
              placeholder="Filter by network (CIDR)"
              value={networkFilter}
              onChange={({ detail }) => setNetworkFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") { setPage(1); load(); }
              }}
            />
            <Input
              placeholder="Filter by client ID (DUID)"
              value={clientIdFilter}
              onChange={({ detail }) => setClientIdFilter(detail.value)}
              onKeyDown={({ detail }) => {
                if (detail.key === "Enter") { setPage(1); load(); }
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
            id: "network",
            header: "Network",
            cell: (item) => item.network ?? "-",
            width: 250,
          },
          {
            id: "source",
            header: "Source",
            cell: (item) => <SourceBadge source={item.source} />,
            width: 120,
          },
          {
            id: "match",
            header: "Match",
            cell: (item) => formatMatch(item.match),
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No reservations</b>
            <Box variant="p" color="inherit">No DHCPv6 reservations found.</Box>
          </Box>
        }
      />
    </SpaceBetween>
  );
}

function SourceBadge({ source }: { source: string }) {
  const color = source === "runtime" ? "#0972d3" : "#656871";
  return (
    <Box display="inline-block" fontSize="body-s" fontWeight="bold">
      <span style={{ color }}>{source}</span>
    </Box>
  );
}

function formatMatch(match: Record<string, unknown>): string {
  return Object.entries(match)
    .map(([k, v]) => `${k}: ${String(v)}`)
    .join(", ");
}

export default function Reservations() {
  return (
    <ContentLayout header={<Header variant="h1">Reservations</Header>}>
      <Tabs
        tabs={[
          { id: "v4", label: "DHCPv4", content: <V4ReservationTable /> },
          { id: "v6", label: "DHCPv6", content: <V6ReservationTable /> },
        ]}
      />
    </ContentLayout>
  );
}
