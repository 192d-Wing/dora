import { useEffect, useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import FormField from "@cloudscape-design/components/form-field";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Modal from "@cloudscape-design/components/modal";
import Select from "@cloudscape-design/components/select";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Table from "@cloudscape-design/components/table";
import Tabs from "@cloudscape-design/components/tabs";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Alert from "@cloudscape-design/components/alert";
import Spinner from "@cloudscape-design/components/spinner";
import Toggle from "@cloudscape-design/components/toggle";
import { api, post, ConfigDocument } from "../api";

interface PoolRow {
  network: string;
  rangeStart: string;
  rangeEnd: string;
  leaseDefault: string;
  leaseMin: string;
  leaseMax: string;
  serverId: string;
  probationPeriod: string;
  rangeIndex: number;
}

interface PoolFormState {
  network: string;
  rangeStart: string;
  rangeEnd: string;
  leaseDefault: string;
  leaseMin: string;
  leaseMax: string;
  serverId: string;
  probationPeriod: string;
  pingCheck: boolean;
}

const EMPTY_V4_FORM: PoolFormState = {
  network: "",
  rangeStart: "",
  rangeEnd: "",
  leaseDefault: "86400",
  leaseMin: "1200",
  leaseMax: "604800",
  serverId: "",
  probationPeriod: "86400",
  pingCheck: false,
};

interface V6PoolRow {
  network: string;
  type: "range" | "pd_pool";
  rangeStart: string;
  rangeEnd: string;
  prefix: string;
  delegatedLen: string;
  leaseDefault: string;
  preferredDefault: string;
  rangeIndex: number;
}

interface V6PoolFormState {
  network: string;
  type: "range" | "pd_pool";
  rangeStart: string;
  rangeEnd: string;
  prefix: string;
  delegatedLen: string;
  leaseDefault: string;
  preferredDefault: string;
}

const EMPTY_V6_FORM: V6PoolFormState = {
  network: "",
  type: "range",
  rangeStart: "",
  rangeEnd: "",
  prefix: "",
  delegatedLen: "64",
  leaseDefault: "3600",
  preferredDefault: "3600",
};

function extractV4Pools(doc: Record<string, unknown>): PoolRow[] {
  const rows: PoolRow[] = [];
  const v4 = doc.v4 as Record<string, unknown> | undefined;
  if (!v4) return rows;
  const networks = v4.networks as Record<string, Record<string, unknown>> | undefined;
  if (!networks) return rows;

  for (const [network, netCfg] of Object.entries(networks)) {
    const ranges = netCfg.ranges as Array<Record<string, unknown>> | undefined;
    const serverId = String(netCfg.server_id ?? "");
    const probationPeriod = String(netCfg.probation_period ?? "");
    if (!ranges || ranges.length === 0) {
      rows.push({
        network,
        rangeStart: "-",
        rangeEnd: "-",
        leaseDefault: "-",
        leaseMin: "-",
        leaseMax: "-",
        serverId,
        probationPeriod,
        rangeIndex: -1,
      });
      continue;
    }
    ranges.forEach((range, idx) => {
      const config = range.config as Record<string, unknown> | undefined;
      const leaseTime = config?.lease_time as Record<string, unknown> | undefined;
      rows.push({
        network,
        rangeStart: String(range.start ?? ""),
        rangeEnd: String(range.end ?? ""),
        leaseDefault: String(leaseTime?.default ?? ""),
        leaseMin: String(leaseTime?.min ?? ""),
        leaseMax: String(leaseTime?.max ?? ""),
        serverId,
        probationPeriod,
        rangeIndex: idx,
      });
    });
  }
  return rows;
}

function extractV6Pools(doc: Record<string, unknown>): V6PoolRow[] {
  const rows: V6PoolRow[] = [];
  const v6 = doc.v6 as Record<string, unknown> | undefined;
  if (!v6) return rows;
  const networks = v6.networks as Record<string, Record<string, unknown>> | undefined;
  if (!networks) return rows;

  for (const [network, netCfg] of Object.entries(networks)) {
    const ranges = netCfg.ranges as Array<Record<string, unknown>> | undefined;
    const pdPools = netCfg.pd_pools as Array<Record<string, unknown>> | undefined;

    if (ranges) {
      ranges.forEach((range, idx) => {
        const config = range.config as Record<string, unknown> | undefined;
        const leaseTime = config?.lease_time as Record<string, unknown> | undefined;
        const preferredTime = config?.preferred_time as Record<string, unknown> | undefined;
        rows.push({
          network,
          type: "range",
          rangeStart: String(range.start ?? ""),
          rangeEnd: String(range.end ?? ""),
          prefix: "",
          delegatedLen: "",
          leaseDefault: String(leaseTime?.default ?? ""),
          preferredDefault: String(preferredTime?.default ?? ""),
          rangeIndex: idx,
        });
      });
    }
    if (pdPools) {
      pdPools.forEach((pool, idx) => {
        const config = pool.config as Record<string, unknown> | undefined;
        const leaseTime = config?.lease_time as Record<string, unknown> | undefined;
        const preferredTime = config?.preferred_time as Record<string, unknown> | undefined;
        rows.push({
          network,
          type: "pd_pool",
          rangeStart: "",
          rangeEnd: "",
          prefix: String(pool.prefix ?? ""),
          delegatedLen: String(pool.delegated_len ?? ""),
          leaseDefault: String(leaseTime?.default ?? ""),
          preferredDefault: String(preferredTime?.default ?? ""),
          rangeIndex: idx,
        });
      });
    }
    if (!ranges?.length && !pdPools?.length) {
      rows.push({
        network,
        type: "range",
        rangeStart: "-",
        rangeEnd: "-",
        prefix: "",
        delegatedLen: "",
        leaseDefault: "-",
        preferredDefault: "-",
        rangeIndex: -1,
      });
    }
  }
  return rows;
}

function applyV4Pool(
  doc: Record<string, unknown>,
  form: PoolFormState,
  editNetwork?: string,
  editRangeIndex?: number
): Record<string, unknown> {
  const clone = JSON.parse(JSON.stringify(doc));
  if (!clone.v4) clone.v4 = {};
  if (!(clone.v4 as Record<string, unknown>).networks)
    (clone.v4 as Record<string, unknown>).networks = {};
  const networks = (clone.v4 as Record<string, unknown>).networks as Record<
    string,
    Record<string, unknown>
  >;

  const newRange: Record<string, unknown> = {
    start: form.rangeStart,
    end: form.rangeEnd,
    config: {
      lease_time: {
        default: parseInt(form.leaseDefault, 10) || 86400,
        min: parseInt(form.leaseMin, 10) || 1200,
        max: parseInt(form.leaseMax, 10) || 604800,
      },
    },
  };

  if (editNetwork != null && editRangeIndex != null && editRangeIndex >= 0) {
    const net = networks[editNetwork];
    if (net) {
      if (form.network !== editNetwork) {
        const ranges = net.ranges as Array<Record<string, unknown>>;
        ranges.splice(editRangeIndex, 1);
        if (ranges.length === 0 && !net.reservations) delete networks[editNetwork];
      }

      if (!networks[form.network]) {
        networks[form.network] = {
          probation_period: parseInt(form.probationPeriod, 10) || 86400,
          ...(form.serverId ? { server_id: form.serverId } : {}),
          ...(form.pingCheck ? { ping_check: true } : {}),
          ranges: [],
        };
      }
      const target = networks[form.network];
      if (!target.ranges) target.ranges = [];

      if (form.network === editNetwork) {
        (target.ranges as Array<Record<string, unknown>>)[editRangeIndex] = newRange;
      } else {
        (target.ranges as Array<Record<string, unknown>>).push(newRange);
      }
    }
  } else {
    if (!networks[form.network]) {
      networks[form.network] = {
        probation_period: parseInt(form.probationPeriod, 10) || 86400,
        ...(form.serverId ? { server_id: form.serverId } : {}),
        ...(form.pingCheck ? { ping_check: true } : {}),
        ranges: [],
      };
    }
    const net = networks[form.network];
    if (!net.ranges) net.ranges = [];
    (net.ranges as Array<Record<string, unknown>>).push(newRange);
  }

  return clone;
}

function applyV6Pool(
  doc: Record<string, unknown>,
  form: V6PoolFormState,
  editNetwork?: string,
  editRangeIndex?: number,
  editType?: "range" | "pd_pool"
): Record<string, unknown> {
  const clone = JSON.parse(JSON.stringify(doc));
  if (!clone.v6) clone.v6 = {};
  if (!(clone.v6 as Record<string, unknown>).networks)
    (clone.v6 as Record<string, unknown>).networks = {};
  const networks = (clone.v6 as Record<string, unknown>).networks as Record<
    string,
    Record<string, unknown>
  >;

  const timeCfg = {
    config: {
      lease_time: { default: parseInt(form.leaseDefault, 10) || 3600 },
      preferred_time: { default: parseInt(form.preferredDefault, 10) || 3600 },
    },
  };

  if (editNetwork != null && editRangeIndex != null && editRangeIndex >= 0) {
    const net = networks[editNetwork];
    if (net) {
      const key = editType === "pd_pool" ? "pd_pools" : "ranges";
      const arr = net[key] as Array<Record<string, unknown>>;
      if (arr) arr.splice(editRangeIndex, 1);
    }
  }

  if (!networks[form.network]) {
    networks[form.network] = {};
  }
  const net = networks[form.network];

  if (form.type === "range") {
    if (!net.ranges) net.ranges = [];
    (net.ranges as Array<Record<string, unknown>>).push({
      start: form.rangeStart,
      end: form.rangeEnd,
      ...timeCfg,
      options: { values: {} },
    });
  } else {
    if (!net.pd_pools) net.pd_pools = [];
    (net.pd_pools as Array<Record<string, unknown>>).push({
      prefix: form.prefix,
      delegated_len: parseInt(form.delegatedLen, 10) || 64,
      ...timeCfg,
    });
  }

  return clone;
}

function V4PoolForm({
  form,
  onChange,
}: {
  form: PoolFormState;
  onChange: (f: PoolFormState) => void;
}) {
  return (
    <SpaceBetween size="m">
      <FormField label="Network (CIDR)">
        <Input
          value={form.network}
          onChange={({ detail }) => onChange({ ...form, network: detail.value })}
          placeholder="192.168.1.0/24"
        />
      </FormField>
      <ColumnLayout columns={2}>
        <FormField label="Range Start">
          <Input
            value={form.rangeStart}
            onChange={({ detail }) => onChange({ ...form, rangeStart: detail.value })}
            placeholder="192.168.1.10"
          />
        </FormField>
        <FormField label="Range End">
          <Input
            value={form.rangeEnd}
            onChange={({ detail }) => onChange({ ...form, rangeEnd: detail.value })}
            placeholder="192.168.1.250"
          />
        </FormField>
      </ColumnLayout>
      <ColumnLayout columns={3}>
        <FormField label="Lease Default (s)">
          <Input
            type="number"
            value={form.leaseDefault}
            onChange={({ detail }) => onChange({ ...form, leaseDefault: detail.value })}
          />
        </FormField>
        <FormField label="Lease Min (s)">
          <Input
            type="number"
            value={form.leaseMin}
            onChange={({ detail }) => onChange({ ...form, leaseMin: detail.value })}
          />
        </FormField>
        <FormField label="Lease Max (s)">
          <Input
            type="number"
            value={form.leaseMax}
            onChange={({ detail }) => onChange({ ...form, leaseMax: detail.value })}
          />
        </FormField>
      </ColumnLayout>
      <ColumnLayout columns={3}>
        <FormField label="Server ID (optional)">
          <Input
            value={form.serverId}
            onChange={({ detail }) => onChange({ ...form, serverId: detail.value })}
            placeholder="192.168.1.1"
          />
        </FormField>
        <FormField label="Probation Period (s)">
          <Input
            type="number"
            value={form.probationPeriod}
            onChange={({ detail }) =>
              onChange({ ...form, probationPeriod: detail.value })
            }
          />
        </FormField>
        <FormField label="Ping Check">
          <Toggle
            checked={form.pingCheck}
            onChange={({ detail }) => onChange({ ...form, pingCheck: detail.checked })}
          >
            {form.pingCheck ? "Enabled" : "Disabled"}
          </Toggle>
        </FormField>
      </ColumnLayout>
    </SpaceBetween>
  );
}

function V6PoolForm({
  form,
  onChange,
}: {
  form: V6PoolFormState;
  onChange: (f: V6PoolFormState) => void;
}) {
  const isRange = form.type === "range";
  return (
    <SpaceBetween size="m">
      <ColumnLayout columns={2}>
        <FormField label="Network (CIDR)">
          <Input
            value={form.network}
            onChange={({ detail }) => onChange({ ...form, network: detail.value })}
            placeholder="2001:db8:1::/64"
          />
        </FormField>
        <FormField label="Pool Type">
          <Select
            selectedOption={{ label: isRange ? "IA_NA Range" : "IA_PD Prefix Delegation", value: form.type }}
            onChange={({ detail }) =>
              onChange({ ...form, type: detail.selectedOption.value as "range" | "pd_pool" })
            }
            options={[
              { label: "IA_NA Range", value: "range" },
              { label: "IA_PD Prefix Delegation", value: "pd_pool" },
            ]}
          />
        </FormField>
      </ColumnLayout>
      {isRange ? (
        <ColumnLayout columns={2}>
          <FormField label="Range Start">
            <Input
              value={form.rangeStart}
              onChange={({ detail }) => onChange({ ...form, rangeStart: detail.value })}
              placeholder="2001:db8:1::100"
            />
          </FormField>
          <FormField label="Range End">
            <Input
              value={form.rangeEnd}
              onChange={({ detail }) => onChange({ ...form, rangeEnd: detail.value })}
              placeholder="2001:db8:1::1ff"
            />
          </FormField>
        </ColumnLayout>
      ) : (
        <ColumnLayout columns={2}>
          <FormField label="Prefix">
            <Input
              value={form.prefix}
              onChange={({ detail }) => onChange({ ...form, prefix: detail.value })}
              placeholder="2001:db8:100::/56"
            />
          </FormField>
          <FormField label="Delegated Length">
            <Input
              type="number"
              value={form.delegatedLen}
              onChange={({ detail }) => onChange({ ...form, delegatedLen: detail.value })}
            />
          </FormField>
        </ColumnLayout>
      )}
      <ColumnLayout columns={2}>
        <FormField label="Lease Default (s)">
          <Input
            type="number"
            value={form.leaseDefault}
            onChange={({ detail }) => onChange({ ...form, leaseDefault: detail.value })}
          />
        </FormField>
        <FormField label="Preferred Time Default (s)">
          <Input
            type="number"
            value={form.preferredDefault}
            onChange={({ detail }) =>
              onChange({ ...form, preferredDefault: detail.value })
            }
          />
        </FormField>
      </ColumnLayout>
    </SpaceBetween>
  );
}

function V4Pools({
  config,
  onSaved,
}: {
  config: ConfigDocument;
  onSaved: () => void;
}) {
  const pools = extractV4Pools(config.document);
  const [modalVisible, setModalVisible] = useState(false);
  const [form, setForm] = useState<PoolFormState>(EMPTY_V4_FORM);
  const [editNetwork, setEditNetwork] = useState<string | undefined>();
  const [editRangeIndex, setEditRangeIndex] = useState<number | undefined>();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const openAdd = () => {
    setForm(EMPTY_V4_FORM);
    setEditNetwork(undefined);
    setEditRangeIndex(undefined);
    setModalVisible(true);
  };

  const openEdit = (row: PoolRow) => {
    setForm({
      network: row.network,
      rangeStart: row.rangeStart === "-" ? "" : row.rangeStart,
      rangeEnd: row.rangeEnd === "-" ? "" : row.rangeEnd,
      leaseDefault: row.leaseDefault === "-" ? "86400" : row.leaseDefault,
      leaseMin: row.leaseMin === "-" ? "1200" : row.leaseMin,
      leaseMax: row.leaseMax === "-" ? "604800" : row.leaseMax,
      serverId: row.serverId,
      probationPeriod: row.probationPeriod || "86400",
      pingCheck: false,
    });
    setEditNetwork(row.network);
    setEditRangeIndex(row.rangeIndex);
    setModalVisible(true);
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    setSuccess(null);
    try {
      const newDoc = applyV4Pool(config.document, form, editNetwork, editRangeIndex);
      await post("/v1/config/candidates", { document: newDoc });
      setSuccess("Configuration candidate staged successfully. Activate it from the Actions page.");
      setModalVisible(false);
      onSaved();
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
      {success && <Alert type="success" dismissible onDismiss={() => setSuccess(null)}>{success}</Alert>}
      <Table
        items={pools}
        trackBy={(item) => `${item.network}-${item.rangeIndex}`}
        variant="full-page"
        stickyHeader
        header={
          <Header
            counter={`(${pools.length})`}
            actions={
              <Button variant="primary" onClick={openAdd}>
                Add pool
              </Button>
            }
          >
            DHCPv4 Pools
          </Header>
        }
        columnDefinitions={[
          { id: "network", header: "Network", cell: (r) => r.network, width: 180 },
          { id: "start", header: "Range Start", cell: (r) => r.rangeStart, width: 160 },
          { id: "end", header: "Range End", cell: (r) => r.rangeEnd, width: 160 },
          { id: "lease", header: "Lease (def/min/max)", cell: (r) => `${r.leaseDefault} / ${r.leaseMin} / ${r.leaseMax}`, width: 200 },
          { id: "server", header: "Server ID", cell: (r) => r.serverId || "-", width: 150 },
          {
            id: "actions",
            header: "Actions",
            cell: (r) => (
              <Button variant="inline-link" onClick={() => openEdit(r)}>
                Edit
              </Button>
            ),
            width: 100,
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No pools</b>
            <Box variant="p" color="inherit">No DHCPv4 pools configured.</Box>
          </Box>
        }
      />
      <Modal
        visible={modalVisible}
        onDismiss={() => setModalVisible(false)}
        header={editNetwork != null ? "Edit DHCPv4 Pool" : "Add DHCPv4 Pool"}
        size="large"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setModalVisible(false)}>
                Cancel
              </Button>
              <Button
                variant="primary"
                loading={saving}
                disabled={!form.network || !form.rangeStart || !form.rangeEnd}
                onClick={save}
              >
                {editNetwork != null ? "Save changes" : "Add pool"}
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <V4PoolForm form={form} onChange={setForm} />
      </Modal>
    </SpaceBetween>
  );
}

function V6Pools({
  config,
  onSaved,
}: {
  config: ConfigDocument;
  onSaved: () => void;
}) {
  const pools = extractV6Pools(config.document);
  const [modalVisible, setModalVisible] = useState(false);
  const [form, setForm] = useState<V6PoolFormState>(EMPTY_V6_FORM);
  const [editNetwork, setEditNetwork] = useState<string | undefined>();
  const [editRangeIndex, setEditRangeIndex] = useState<number | undefined>();
  const [editType, setEditType] = useState<"range" | "pd_pool" | undefined>();
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const openAdd = () => {
    setForm(EMPTY_V6_FORM);
    setEditNetwork(undefined);
    setEditRangeIndex(undefined);
    setEditType(undefined);
    setModalVisible(true);
  };

  const openEdit = (row: V6PoolRow) => {
    setForm({
      network: row.network,
      type: row.type,
      rangeStart: row.rangeStart === "-" ? "" : row.rangeStart,
      rangeEnd: row.rangeEnd === "-" ? "" : row.rangeEnd,
      prefix: row.prefix,
      delegatedLen: row.delegatedLen || "64",
      leaseDefault: row.leaseDefault === "-" ? "3600" : row.leaseDefault,
      preferredDefault: row.preferredDefault === "-" ? "3600" : row.preferredDefault,
    });
    setEditNetwork(row.network);
    setEditRangeIndex(row.rangeIndex);
    setEditType(row.type);
    setModalVisible(true);
  };

  const save = async () => {
    setSaving(true);
    setError(null);
    setSuccess(null);
    try {
      const newDoc = applyV6Pool(config.document, form, editNetwork, editRangeIndex, editType);
      await post("/v1/config/candidates", { document: newDoc });
      setSuccess("Configuration candidate staged successfully. Activate it from the Actions page.");
      setModalVisible(false);
      onSaved();
    } catch (err) {
      setError(String(err));
    } finally {
      setSaving(false);
    }
  };

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
      {success && <Alert type="success" dismissible onDismiss={() => setSuccess(null)}>{success}</Alert>}
      <Table
        items={pools}
        trackBy={(item) => `${item.network}-${item.type}-${item.rangeIndex}`}
        variant="full-page"
        stickyHeader
        header={
          <Header
            counter={`(${pools.length})`}
            actions={
              <Button variant="primary" onClick={openAdd}>
                Add pool
              </Button>
            }
          >
            DHCPv6 Pools
          </Header>
        }
        columnDefinitions={[
          { id: "network", header: "Network", cell: (r) => r.network, width: 220 },
          {
            id: "type",
            header: "Type",
            cell: (r) => (r.type === "pd_pool" ? "IA_PD" : "IA_NA"),
            width: 80,
          },
          {
            id: "range",
            header: "Range / Prefix",
            cell: (r) =>
              r.type === "pd_pool"
                ? `${r.prefix} /${r.delegatedLen}`
                : `${r.rangeStart} — ${r.rangeEnd}`,
            width: 300,
          },
          {
            id: "lease",
            header: "Lease / Preferred (s)",
            cell: (r) => `${r.leaseDefault} / ${r.preferredDefault}`,
            width: 180,
          },
          {
            id: "actions",
            header: "Actions",
            cell: (r) => (
              <Button variant="inline-link" onClick={() => openEdit(r)}>
                Edit
              </Button>
            ),
            width: 100,
          },
        ]}
        empty={
          <Box textAlign="center" color="inherit">
            <b>No pools</b>
            <Box variant="p" color="inherit">No DHCPv6 pools configured.</Box>
          </Box>
        }
      />
      <Modal
        visible={modalVisible}
        onDismiss={() => setModalVisible(false)}
        header={editNetwork != null ? "Edit DHCPv6 Pool" : "Add DHCPv6 Pool"}
        size="large"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setModalVisible(false)}>
                Cancel
              </Button>
              <Button
                variant="primary"
                loading={saving}
                disabled={
                  !form.network ||
                  (form.type === "range" && (!form.rangeStart || !form.rangeEnd)) ||
                  (form.type === "pd_pool" && !form.prefix)
                }
                onClick={save}
              >
                {editNetwork != null ? "Save changes" : "Add pool"}
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <V6PoolForm form={form} onChange={setForm} />
      </Modal>
    </SpaceBetween>
  );
}

export default function Pools() {
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
          description="Manage DHCP address pools"
          actions={<Button iconName="refresh" onClick={load} />}
        >
          Pools
        </Header>
      }
    >
      <SpaceBetween size="l">
        {error && <Alert type="error">{error}</Alert>}
        {config && (
          <Tabs
            tabs={[
              {
                id: "v4",
                label: "DHCPv4",
                content: <V4Pools config={config} onSaved={load} />,
              },
              {
                id: "v6",
                label: "DHCPv6",
                content: <V6Pools config={config} onSaved={load} />,
              },
            ]}
          />
        )}
      </SpaceBetween>
    </ContentLayout>
  );
}
