import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import FormField from "@cloudscape-design/components/form-field";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Modal from "@cloudscape-design/components/modal";
import Pagination from "@cloudscape-design/components/pagination";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Table from "@cloudscape-design/components/table";
import Tabs from "@cloudscape-design/components/tabs";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Alert from "@cloudscape-design/components/alert";
import { api, V4Reservation, V6Reservation } from "../api";
import { useNotifications } from "../components/Notifications";
import { toCsv, downloadCsv } from "../utils/csv";

const PAGE_SIZE = 50;

const IPV4_RE = /^(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)$/;
const IPV6_RE = /^([0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}$/;

interface MatchEntry {
  key: string;
  value: string;
}

function MatchEditor({ entries, onChange }: { entries: MatchEntry[]; onChange: (e: MatchEntry[]) => void }) {
  const add = () => onChange([...entries, { key: "", value: "" }]);
  const remove = (idx: number) => onChange(entries.filter((_, i) => i !== idx));
  const update = (idx: number, field: "key" | "value", val: string) =>
    onChange(entries.map((e, i) => (i === idx ? { ...e, [field]: val } : e)));

  return (
    <SpaceBetween size="xs">
      <Box variant="awsui-key-label">Match Criteria</Box>
      {entries.map((entry, idx) => (
        <div key={idx} style={{ display: "grid", gridTemplateColumns: "1fr 2fr auto", gap: "12px" }}>
          <Input
            value={entry.key}
            onChange={({ detail }) => update(idx, "key", detail.value)}
            placeholder="e.g. chaddr"
          />
          <Input
            value={entry.value}
            onChange={({ detail }) => update(idx, "value", detail.value)}
            placeholder="e.g. aa:bb:cc:dd:ee:ff"
          />
          <Button variant="icon" iconName="remove" onClick={() => remove(idx)} />
        </div>
      ))}
      <Button variant="normal" iconName="add-plus" onClick={add}>
        Add match rule
      </Button>
      {entries.length === 0 && (
        <Box fontSize="body-s" color="text-status-inactive">
          Common keys: chaddr (MAC), client_id, circuit_id, remote_id
        </Box>
      )}
    </SpaceBetween>
  );
}

function V4ReservationTable() {
  const { notify } = useNotifications();
  const [items, setItems] = useState<V4Reservation[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [ipFilter, setIpFilter] = useState("");
  const [networkFilter, setNetworkFilter] = useState("");
  const [clientIdFilter, setClientIdFilter] = useState("");

  const [createVisible, setCreateVisible] = useState(false);
  const [createIp, setCreateIp] = useState("");
  const [createMatch, setCreateMatch] = useState<MatchEntry[]>([{ key: "chaddr", value: "" }]);
  const [creating, setCreating] = useState(false);
  const [showErrors, setShowErrors] = useState(false);

  const [deleteTarget, setDeleteTarget] = useState<V4Reservation | null>(null);
  const [deleting, setDeleting] = useState(false);

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

  const openCreate = () => {
    setCreateIp("");
    setCreateMatch([{ key: "chaddr", value: "" }]);
    setShowErrors(false);
    setCreateVisible(true);
  };

  const ipErr = showErrors && !IPV4_RE.test(createIp) ? "Enter a valid IPv4 address" : undefined;
  const matchErr = showErrors && createMatch.filter((m) => m.key && m.value).length === 0
    ? "At least one match rule is required" : undefined;

  const submitCreate = async () => {
    setShowErrors(true);
    if (!IPV4_RE.test(createIp)) return;
    const validMatch = createMatch.filter((m) => m.key && m.value);
    if (validMatch.length === 0) return;

    const match: Record<string, string> = {};
    for (const m of validMatch) match[m.key] = m.value;

    setCreating(true);
    setError(null);
    try {
      await api.createReservation("v4", { family: "v4", ip: createIp, match });
      notify("success", `Reservation ${createIp} created.`);
      setCreateVisible(false);
      load();
    } catch (err) {
      setError(String(err));
    } finally {
      setCreating(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteTarget) return;
    setDeleting(true);
    setError(null);
    try {
      await api.deleteReservation("v4", deleteTarget.ip);
      notify("success", `Reservation ${deleteTarget.ip} deleted.`);
      setDeleteTarget(null);
      load();
    } catch (err) {
      setError(String(err));
    } finally {
      setDeleting(false);
    }
  };

  const exportCsv = async () => {
    const params: Record<string, string> = { limit: "1000", offset: "0" };
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;
    const res = await api.reservationsV4(params);
    const headers = ["IP", "Network", "Source", "Match"];
    const rows = res.items.map((r) => [
      r.ip, r.network ?? "", r.source, formatMatch(r.match),
    ]);
    downloadCsv("dhcpv4-reservations.csv", toCsv(headers, rows));
  };

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
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
            actions={
              <SpaceBetween direction="horizontal" size="xs">
                <Button iconName="download" onClick={exportCsv}>Export CSV</Button>
                <Button iconName="refresh" onClick={load} />
                <Button variant="primary" onClick={openCreate}>
                  Add reservation
                </Button>
              </SpaceBetween>
            }
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
          {
            id: "actions",
            header: "Actions",
            cell: (item) =>
              item.source === "runtime" ? (
                <Button variant="inline-link" onClick={() => setDeleteTarget(item)}>
                  Delete
                </Button>
              ) : (
                <Box color="text-status-inactive" fontSize="body-s">config</Box>
              ),
            width: 100,
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No reservations</b>
            <Box variant="p" color="inherit">No DHCPv4 reservations found.</Box>
          </Box>
        }
      />

      <Modal
        visible={createVisible}
        onDismiss={() => setCreateVisible(false)}
        header="Add DHCPv4 Reservation"
        size="medium"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setCreateVisible(false)}>Cancel</Button>
              <Button variant="primary" loading={creating} onClick={submitCreate}>
                Create reservation
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <SpaceBetween size="m">
          <FormField label="IP Address" errorText={ipErr}>
            <Input
              value={createIp}
              onChange={({ detail }) => setCreateIp(detail.value)}
              placeholder="192.168.1.100"
              invalid={!!ipErr}
            />
          </FormField>
          {matchErr && <Alert type="error">{matchErr}</Alert>}
          <MatchEditor entries={createMatch} onChange={setCreateMatch} />
        </SpaceBetween>
      </Modal>

      <Modal
        visible={deleteTarget !== null}
        onDismiss={() => setDeleteTarget(null)}
        header="Delete Reservation"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setDeleteTarget(null)}>Cancel</Button>
              <Button variant="primary" loading={deleting} onClick={confirmDelete}>Delete</Button>
            </SpaceBetween>
          </Box>
        }
      >
        Delete reservation for <strong>{deleteTarget?.ip}</strong>?{" "}
        Match: <strong>{deleteTarget ? formatMatch(deleteTarget.match) : ""}</strong>
      </Modal>
    </SpaceBetween>
  );
}

function V6ReservationTable() {
  const { notify } = useNotifications();
  const [items, setItems] = useState<V6Reservation[]>([]);
  const [total, setTotal] = useState(0);
  const [page, setPage] = useState(1);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [ipFilter, setIpFilter] = useState("");
  const [networkFilter, setNetworkFilter] = useState("");
  const [clientIdFilter, setClientIdFilter] = useState("");

  const [createVisible, setCreateVisible] = useState(false);
  const [createIp, setCreateIp] = useState("");
  const [createMatch, setCreateMatch] = useState<MatchEntry[]>([{ key: "duid", value: "" }]);
  const [creating, setCreating] = useState(false);
  const [showErrors, setShowErrors] = useState(false);

  const [deleteTarget, setDeleteTarget] = useState<V6Reservation | null>(null);
  const [deleting, setDeleting] = useState(false);

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

  const openCreate = () => {
    setCreateIp("");
    setCreateMatch([{ key: "duid", value: "" }]);
    setShowErrors(false);
    setCreateVisible(true);
  };

  const ipErr = showErrors && !IPV6_RE.test(createIp) ? "Enter a valid IPv6 address" : undefined;
  const matchErr = showErrors && createMatch.filter((m) => m.key && m.value).length === 0
    ? "At least one match rule is required" : undefined;

  const submitCreate = async () => {
    setShowErrors(true);
    if (!IPV6_RE.test(createIp)) return;
    const validMatch = createMatch.filter((m) => m.key && m.value);
    if (validMatch.length === 0) return;

    const match: Record<string, string> = {};
    for (const m of validMatch) match[m.key] = m.value;

    setCreating(true);
    setError(null);
    try {
      await api.createReservation("v6", { family: "v6", ip: createIp, match });
      notify("success", `Reservation ${createIp} created.`);
      setCreateVisible(false);
      load();
    } catch (err) {
      setError(String(err));
    } finally {
      setCreating(false);
    }
  };

  const confirmDelete = async () => {
    if (!deleteTarget?.ip) return;
    setDeleting(true);
    setError(null);
    try {
      await api.deleteReservation("v6", deleteTarget.ip);
      notify("success", `Reservation ${deleteTarget.ip} deleted.`);
      setDeleteTarget(null);
      load();
    } catch (err) {
      setError(String(err));
    } finally {
      setDeleting(false);
    }
  };

  const exportCsv = async () => {
    const params: Record<string, string> = { limit: "1000", offset: "0" };
    if (ipFilter) params.ip = ipFilter;
    if (networkFilter) params.network = networkFilter;
    if (clientIdFilter) params.client_id = clientIdFilter;
    const res = await api.reservationsV6(params);
    const headers = ["IP", "Prefix", "Network", "Source", "Match"];
    const rows = res.items.map((r) => [
      r.ip ?? "", r.prefix ?? "", r.network ?? "", r.source, formatMatch(r.match),
    ]);
    downloadCsv("dhcpv6-reservations.csv", toCsv(headers, rows));
  };

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
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
            actions={
              <SpaceBetween direction="horizontal" size="xs">
                <Button iconName="download" onClick={exportCsv}>Export CSV</Button>
                <Button iconName="refresh" onClick={load} />
                <Button variant="primary" onClick={openCreate}>
                  Add reservation
                </Button>
              </SpaceBetween>
            }
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
          {
            id: "actions",
            header: "Actions",
            cell: (item) =>
              item.source === "runtime" && item.ip ? (
                <Button variant="inline-link" onClick={() => setDeleteTarget(item)}>
                  Delete
                </Button>
              ) : (
                <Box color="text-status-inactive" fontSize="body-s">
                  {item.source === "config" ? "config" : "-"}
                </Box>
              ),
            width: 100,
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No reservations</b>
            <Box variant="p" color="inherit">No DHCPv6 reservations found.</Box>
          </Box>
        }
      />

      <Modal
        visible={createVisible}
        onDismiss={() => setCreateVisible(false)}
        header="Add DHCPv6 Reservation"
        size="medium"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setCreateVisible(false)}>Cancel</Button>
              <Button variant="primary" loading={creating} onClick={submitCreate}>
                Create reservation
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <SpaceBetween size="m">
          <FormField label="IPv6 Address" errorText={ipErr}>
            <Input
              value={createIp}
              onChange={({ detail }) => setCreateIp(detail.value)}
              placeholder="2001:db8:1::100"
              invalid={!!ipErr}
            />
          </FormField>
          {matchErr && <Alert type="error">{matchErr}</Alert>}
          <MatchEditor entries={createMatch} onChange={setCreateMatch} />
        </SpaceBetween>
      </Modal>

      <Modal
        visible={deleteTarget !== null}
        onDismiss={() => setDeleteTarget(null)}
        header="Delete Reservation"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setDeleteTarget(null)}>Cancel</Button>
              <Button variant="primary" loading={deleting} onClick={confirmDelete}>Delete</Button>
            </SpaceBetween>
          </Box>
        }
      >
        Delete reservation for <strong>{deleteTarget?.ip}</strong>?{" "}
        Match: <strong>{deleteTarget ? formatMatch(deleteTarget.match) : ""}</strong>
      </Modal>
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
    .map(([k, v]) => {
      const display = typeof v === "object" && v !== null ? JSON.stringify(v) : String(v);
      return `${k}: ${display}`;
    })
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
