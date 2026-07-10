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
import TokenGroup from "@cloudscape-design/components/token-group";
import TextFilter from "@cloudscape-design/components/text-filter";
import { api, post, ConfigDocument } from "../api";

interface OptionEntry {
  code: string;
  type: string;
  values: string[];
}

const OPTION_TYPES = [
  { label: "ip", value: "ip" },
  { label: "str", value: "str" },
  { label: "domain", value: "domain" },
  { label: "u8", value: "u8" },
  { label: "u16", value: "u16" },
  { label: "u32", value: "u32" },
  { label: "i32", value: "i32" },
  { label: "b64", value: "b64" },
  { label: "hex", value: "hex" },
];

const COMMON_V4_OPTIONS: Record<string, string> = {
  "1": "Subnet Mask",
  "2": "Time Offset",
  "3": "Router",
  "4": "Time Server",
  "5": "Name Server",
  "6": "Domain Server",
  "7": "Log Server",
  "8": "Quotes Server",
  "9": "LPR Server",
  "10": "Impress Server",
  "11": "RLP Server",
  "12": "Hostname",
  "13": "Boot File Size",
  "14": "Merit Dump File",
  "15": "Domain Name",
  "16": "Swap Server",
  "17": "Root Path",
  "18": "Extension File",
  "19": "Forward On/Off",
  "20": "SrcRte On/Off",
  "21": "Policy Filter",
  "22": "Max DG Assembly",
  "23": "Default IP TTL",
  "24": "MTU Timeout",
  "25": "MTU Plateau",
  "26": "MTU Interface",
  "27": "MTU Subnet",
  "28": "Broadcast Address",
  "29": "Mask Discovery",
  "30": "Mask Supplier",
  "31": "Router Discovery",
  "32": "Router Request",
  "33": "Static Route",
  "34": "Trailers",
  "35": "ARP Timeout",
  "36": "Ethernet",
  "37": "Default TCP TTL",
  "38": "Keepalive Time",
  "39": "Keepalive Data",
  "40": "NIS Domain",
  "41": "NIS Servers",
  "42": "NTP Servers",
  "43": "Vendor Specific",
  "44": "NETBIOS Name Srv",
  "45": "NETBIOS Dist Srv",
  "46": "NETBIOS Node Type",
  "47": "NETBIOS Scope",
  "48": "X Window Font",
  "49": "X Window Manager",
  "50": "Address Request",
  "51": "Address Time",
  "52": "Overload",
  "53": "DHCP Msg Type",
  "54": "DHCP Server Id",
  "55": "Parameter List",
  "56": "DHCP Message",
  "57": "DHCP Max Msg Size",
  "58": "Renewal Time",
  "59": "Rebinding Time",
  "60": "Class Id",
  "61": "Client Id",
  "62": "NetWare/IP Domain",
  "63": "NetWare/IP Option",
  "64": "NIS-Domain-Name",
  "65": "NIS-Server-Addr",
  "66": "Server-Name",
  "67": "Bootfile-Name",
  "68": "Home-Agent-Addrs",
  "69": "SMTP-Server",
  "70": "POP3-Server",
  "71": "NNTP-Server",
  "72": "WWW-Server",
  "73": "Finger-Server",
  "74": "IRC-Server",
  "75": "StreetTalk-Server",
  "76": "STDA-Server",
  "77": "User-Class",
  "78": "Directory Agent",
  "79": "Service Scope",
  "80": "Rapid Commit",
  "81": "Client FQDN",
  "82": "Relay Agent Info",
  "83": "iSNS",
  "85": "NDS Servers",
  "86": "NDS Tree Name",
  "87": "NDS Context",
  "88": "BCMCS Ctrl Domain",
  "89": "BCMCS Ctrl Addr",
  "90": "Authentication",
  "91": "Last Transaction",
  "92": "Associated IP",
  "93": "Client System",
  "94": "Client NDI",
  "95": "LDAP",
  "97": "UUID/GUID",
  "98": "User-Auth",
  "99": "GEOCONF_CIVIC",
  "100": "PCode",
  "101": "TCode",
  "108": "IPv6-Only Preferred",
  "112": "Netinfo Address",
  "113": "Netinfo Tag",
  "114": "Captive-Portal",
  "116": "Auto-Config",
  "117": "Name Svc Search",
  "118": "Subnet Selection",
  "119": "Domain Search",
  "120": "SIP Servers",
  "121": "Classless Static Route",
  "122": "CCC",
  "123": "GeoConf",
  "124": "V-I Vendor Class",
  "125": "V-I Vendor-Specific",
  "128": "PXE (vendor)",
  "129": "PXE (vendor)",
  "130": "PXE (vendor)",
  "131": "PXE (vendor)",
  "132": "PXE (vendor)",
  "133": "PXE (vendor)",
  "134": "PXE (vendor)",
  "135": "PXE (vendor)",
  "136": "PANA Agent",
  "137": "V4 LOST",
  "138": "CAPWAP AC",
  "139": "IPv4 Addr MoS",
  "140": "IPv4 FQDN MoS",
  "141": "SIP UA CS Domains",
  "142": "IPv4 Addr ANDSF",
  "143": "V4 SZTP Redirect",
  "144": "GeoLoc",
  "145": "Forcerenew Nonce",
  "146": "RDNSS Selection",
  "150": "TFTP Server Addr",
  "209": "Configuration File",
  "210": "Path Prefix",
  "211": "Reboot Time",
  "212": "6RD",
  "213": "V4 Access Domain",
  "220": "Subnet Allocation",
  "221": "Virtual Subnet Selection",
};

const COMMON_V6_OPTIONS: Record<string, string> = {
  "1": "Client ID",
  "2": "Server ID",
  "3": "IA_NA",
  "4": "IA_TA",
  "5": "IA Address",
  "6": "Option Request",
  "7": "Preference",
  "8": "Elapsed Time",
  "9": "Relay Msg",
  "11": "Auth",
  "12": "Unicast",
  "13": "Status Code",
  "14": "Rapid Commit",
  "15": "User Class",
  "16": "Vendor Class",
  "17": "Vendor Opts",
  "18": "Interface ID",
  "19": "Reconf Msg",
  "20": "Reconf Accept",
  "21": "SIP Server Names",
  "22": "SIP Server Addrs",
  "23": "DNS Servers",
  "24": "Domain List",
  "25": "IA_PD",
  "26": "IA Prefix",
  "27": "NIS Servers",
  "28": "NISP Servers",
  "29": "NIS Domain Name",
  "30": "NISP Domain Name",
  "31": "SNTP Servers",
  "32": "Info Refresh Time",
  "33": "BCMCS Server D",
  "34": "BCMCS Server A",
  "36": "GEOCONF_CIVIC",
  "37": "Remote ID",
  "38": "Subscriber ID",
  "39": "Client FQDN",
  "40": "PANA Agent",
  "41": "POSIX Timezone",
  "42": "TZDB Timezone",
  "43": "ERO",
  "56": "NTP Server",
  "57": "V6 Access Domain",
  "58": "SIP UA CS List",
  "59": "Boot File URL",
  "60": "Boot File Param",
  "61": "Client Arch Type",
  "62": "NII",
  "63": "Geolocation",
  "64": "AFTR Name",
  "65": "ERP Local Domain",
  "74": "RDNSS Selection",
  "79": "Client Link-Layer",
  "82": "Sol Max RT",
  "83": "Inf Max RT",
  "86": "V6 PCP Server",
  "103": "Captive-Portal",
  "112": "MUD URL",
  "136": "V6 SZTP Redirect",
  "143": "IPv6 Addr ANDSF",
  "144": "V6 DNR",
};

function optionLabel(code: string, family: "v4" | "v6"): string {
  const map = family === "v4" ? COMMON_V4_OPTIONS : COMMON_V6_OPTIONS;
  const name = map[code];
  return name ? `${code} (${name})` : code;
}

const MULTI_VALUE_TYPES = new Set(["ip", "domain"]);

function extractOptions(opts: Record<string, unknown> | undefined): OptionEntry[] {
  if (!opts) return [];
  const valuesMap = opts.values as Record<string, Record<string, unknown>> | undefined;
  if (!valuesMap) return [];
  return Object.entries(valuesMap).map(([code, opt]) => {
    let vals: string[];
    if (Array.isArray(opt.value)) {
      vals = opt.value.map((v: unknown) =>
        typeof v === "string" || typeof v === "number" ? String(v) : ""
      ).filter(Boolean);
    } else if (typeof opt.value === "string" || typeof opt.value === "number") {
      vals = [String(opt.value)];
    } else {
      vals = [];
    }
    return {
      code,
      type: typeof opt.type === "string" ? opt.type : "ip",
      values: vals,
    };
  });
}

function buildOptionsObj(entries: OptionEntry[]): Record<string, unknown> {
  const out: Record<string, unknown> = {};
  for (const entry of entries) {
    if (!entry.code) continue;
    const val = entry.values.length === 1 ? entry.values[0] : entry.values;
    out[entry.code] = { type: entry.type, value: val };
  }
  return { values: out };
}

function TokenValueInput({
  values,
  onChange,
  placeholder,
}: {
  values: string[];
  onChange: (vals: string[]) => void;
  placeholder: string;
}) {
  const [draft, setDraft] = useState("");

  const addValue = () => {
    const trimmed = draft.trim();
    if (!trimmed) return;
    onChange([...values, trimmed]);
    setDraft("");
  };

  return (
    <SpaceBetween size="xxs">
      <div style={{ display: "flex", gap: "6px" }}>
        <div style={{ flex: 1 }}>
          <Input
            value={draft}
            onChange={({ detail }) => setDraft(detail.value)}
            onKeyDown={({ detail }) => {
              if (detail.key === "Enter") addValue();
            }}
            placeholder={placeholder}
          />
        </div>
        <Button variant="icon" iconName="add-plus" onClick={addValue} />
      </div>
      {values.length > 0 && (
        <TokenGroup
          items={values.map((v, i) => ({ label: v, dismissLabel: `Remove ${v}`, tag: String(i) }))}
          onDismiss={({ detail }) =>
            onChange(values.filter((_, i) => i !== detail.itemIndex))
          }
        />
      )}
    </SpaceBetween>
  );
}

function OptionsEditor({
  options,
  onChange,
  family,
}: {
  options: OptionEntry[];
  onChange: (opts: OptionEntry[]) => void;
  family: "v4" | "v6";
}) {
  const addOption = () =>
    onChange([...options, { code: "", type: "ip", values: [] }]);

  const removeOption = (idx: number) =>
    onChange(options.filter((_, i) => i !== idx));

  const updateField = (idx: number, field: "code" | "type", val: string) => {
    const updated = options.map((opt, i) =>
      i === idx ? { ...opt, [field]: val } : opt
    );
    onChange(updated);
  };

  const updateValues = (idx: number, vals: string[]) => {
    const updated = options.map((opt, i) =>
      i === idx ? { ...opt, values: vals } : opt
    );
    onChange(updated);
  };

  const placeholderForType = (type: string) => {
    if (type === "ip") return "e.g. 8.8.8.8";
    if (type === "domain") return "e.g. example.com.";
    if (type === "str") return "e.g. myvalue";
    if (type === "hex") return "e.g. DEADBEEF";
    if (type === "b64") return "e.g. Zm9vYmFy";
    return "value";
  };

  return (
    <SpaceBetween size="s">
      <Box variant="awsui-key-label">DHCP Options</Box>
      {options.map((opt, idx) => (
        <div key={idx} style={{ display: "grid", gridTemplateColumns: "1fr 1fr 2fr auto", gap: "20px" }}>
          <div>
            <FormField label={idx === 0 ? "Option Code" : undefined}>
              <Input
                value={opt.code}
                onChange={({ detail }) => updateField(idx, "code", detail.value)}
                placeholder="e.g. 6"
              />
            </FormField>
            {opt.code && (
              <Box fontSize="body-s" color="text-status-info">
                {optionLabel(opt.code, family)}
              </Box>
            )}
          </div>
          <FormField label={idx === 0 ? "Type" : undefined}>
            <Select
              selectedOption={OPTION_TYPES.find((t) => t.value === opt.type) ?? OPTION_TYPES[0]}
              onChange={({ detail }) =>
                updateField(idx, "type", detail.selectedOption.value ?? "ip")
              }
              options={OPTION_TYPES}
            />
          </FormField>
          <FormField label={idx === 0 ? "Value(s)" : undefined}>
            {MULTI_VALUE_TYPES.has(opt.type) ? (
              <TokenValueInput
                values={opt.values}
                onChange={(vals) => updateValues(idx, vals)}
                placeholder={placeholderForType(opt.type)}
              />
            ) : (
              <Input
                value={opt.values[0] ?? ""}
                onChange={({ detail }) => updateValues(idx, detail.value ? [detail.value] : [])}
                placeholder={placeholderForType(opt.type)}
              />
            )}
          </FormField>
          <div style={{ marginTop: idx === 0 ? "26px" : "0" }}>
            <Button variant="icon" iconName="remove" onClick={() => removeOption(idx)} />
          </div>
        </div>
      ))}
      <Button variant="normal" iconName="add-plus" onClick={addOption}>
        Add option
      </Button>
    </SpaceBetween>
  );
}

interface PoolRow {
  name: string;
  network: string;
  rangeStart: string;
  rangeEnd: string;
  leaseDefault: string;
  leaseMin: string;
  leaseMax: string;
  serverId: string;
  probationPeriod: string;
  rangeIndex: number;
  options: OptionEntry[];
}

interface PoolFormState {
  name: string;
  network: string;
  rangeStart: string;
  rangeEnd: string;
  leaseDefault: string;
  leaseMin: string;
  leaseMax: string;
  serverId: string;
  probationPeriod: string;
  pingCheck: boolean;
  options: OptionEntry[];
}

const EMPTY_V4_FORM: PoolFormState = {
  name: "",
  network: "",
  rangeStart: "",
  rangeEnd: "",
  leaseDefault: "86400",
  leaseMin: "1200",
  leaseMax: "604800",
  serverId: "",
  probationPeriod: "86400",
  pingCheck: false,
  options: [],
};

interface V6PoolRow {
  name: string;
  network: string;
  type: "range" | "pd_pool";
  rangeStart: string;
  rangeEnd: string;
  prefix: string;
  delegatedLen: string;
  leaseDefault: string;
  preferredDefault: string;
  rangeIndex: number;
  options: OptionEntry[];
}

interface V6PoolFormState {
  name: string;
  network: string;
  type: "range" | "pd_pool";
  rangeStart: string;
  rangeEnd: string;
  prefix: string;
  delegatedLen: string;
  leaseDefault: string;
  preferredDefault: string;
  options: OptionEntry[];
}

const EMPTY_V6_FORM: V6PoolFormState = {
  name: "",
  network: "",
  type: "range",
  rangeStart: "",
  rangeEnd: "",
  prefix: "",
  delegatedLen: "64",
  leaseDefault: "3600",
  preferredDefault: "3600",
  options: [],
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
        name: typeof netCfg.name === "string" ? netCfg.name : "",
        network,
        rangeStart: "-",
        rangeEnd: "-",
        leaseDefault: "-",
        leaseMin: "-",
        leaseMax: "-",
        serverId,
        probationPeriod,
        rangeIndex: -1,
        options: [],
      });
      continue;
    }
    ranges.forEach((range, idx) => {
      const config = range.config as Record<string, unknown> | undefined;
      const leaseTime = config?.lease_time as Record<string, unknown> | undefined;
      const opts = range.options as Record<string, unknown> | undefined;
      rows.push({
        name: (typeof range.name === "string" && range.name) || (typeof netCfg.name === "string" && netCfg.name) || "",
        network,
        rangeStart: String(range.start ?? ""),
        rangeEnd: String(range.end ?? ""),
        leaseDefault: String(leaseTime?.default ?? ""),
        leaseMin: String(leaseTime?.min ?? ""),
        leaseMax: String(leaseTime?.max ?? ""),
        serverId,
        probationPeriod,
        rangeIndex: idx,
        options: extractOptions(opts),
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
        const opts = range.options as Record<string, unknown> | undefined;
        const rangeName = (typeof range.name === "string" && range.name)
          || (typeof netCfg.name === "string" && netCfg.name) || "";
        rows.push({
          name: rangeName,
          network,
          type: "range",
          rangeStart: String(range.start ?? ""),
          rangeEnd: String(range.end ?? ""),
          prefix: "",
          delegatedLen: "",
          leaseDefault: String(leaseTime?.default ?? ""),
          preferredDefault: String(preferredTime?.default ?? ""),
          rangeIndex: idx,
          options: extractOptions(opts),
        });
      });
    }
    if (pdPools) {
      pdPools.forEach((pool, idx) => {
        const config = pool.config as Record<string, unknown> | undefined;
        const leaseTime = config?.lease_time as Record<string, unknown> | undefined;
        const preferredTime = config?.preferred_time as Record<string, unknown> | undefined;
        const opts = pool.options as Record<string, unknown> | undefined;
        const poolName = (typeof pool.name === "string" && pool.name)
          || (typeof netCfg.name === "string" && netCfg.name) || "";
        rows.push({
          name: poolName,
          network,
          type: "pd_pool",
          rangeStart: "",
          rangeEnd: "",
          prefix: String(pool.prefix ?? ""),
          delegatedLen: String(pool.delegated_len ?? ""),
          leaseDefault: String(leaseTime?.default ?? ""),
          preferredDefault: String(preferredTime?.default ?? ""),
          rangeIndex: idx,
          options: extractOptions(opts),
        });
      });
    }
    if (!ranges?.length && !pdPools?.length) {
      rows.push({
        name: typeof netCfg.name === "string" ? netCfg.name : "",
        network,
        type: "range",
        rangeStart: "-",
        rangeEnd: "-",
        prefix: "",
        delegatedLen: "",
        leaseDefault: "-",
        preferredDefault: "-",
        rangeIndex: -1,
        options: [],
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
    ...(form.name ? { name: form.name } : {}),
    start: form.rangeStart,
    end: form.rangeEnd,
    config: {
      lease_time: {
        default: parseInt(form.leaseDefault, 10) || 86400,
        min: parseInt(form.leaseMin, 10) || 1200,
        max: parseInt(form.leaseMax, 10) || 604800,
      },
    },
    ...(form.options.length > 0 ? { options: buildOptionsObj(form.options) } : {}),
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

  const optsPart = form.options.length > 0 ? { options: buildOptionsObj(form.options) } : {};

  if (form.type === "range") {
    if (!net.ranges) net.ranges = [];
    (net.ranges as Array<Record<string, unknown>>).push({
      ...(form.name ? { name: form.name } : {}),
      start: form.rangeStart,
      end: form.rangeEnd,
      ...timeCfg,
      ...optsPart,
    });
  } else {
    if (!net.pd_pools) net.pd_pools = [];
    (net.pd_pools as Array<Record<string, unknown>>).push({
      ...(form.name ? { name: form.name } : {}),
      prefix: form.prefix,
      delegated_len: parseInt(form.delegatedLen, 10) || 64,
      ...timeCfg,
      ...optsPart,
    });
  }

  return clone;
}

const IPV4_RE = /^(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)$/;
const IPV4_CIDR_RE = /^(?:(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\.){3}(?:25[0-5]|2[0-4]\d|[01]?\d\d?)\/(?:[0-9]|[12]\d|3[0-2])$/;
const IPV6_RE = /^([0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}$/;
const IPV6_CIDR_RE = /^([0-9a-fA-F]{0,4}:){2,7}[0-9a-fA-F]{0,4}\/\d{1,3}$/;

function validateIpv4(value: string): string | undefined {
  if (!value) return "Required";
  if (!IPV4_RE.test(value)) return "Enter a valid IPv4 address (e.g. 192.168.1.10)";
  return undefined;
}

function validateIpv4Cidr(value: string): string | undefined {
  if (!value) return "Required";
  if (!IPV4_CIDR_RE.test(value)) return "Enter a valid IPv4 CIDR (e.g. 192.168.1.0/24)";
  return undefined;
}

function validateIpv6(value: string): string | undefined {
  if (!value) return "Required";
  if (!IPV6_RE.test(value)) return "Enter a valid IPv6 address (e.g. 2001:db8:1::100)";
  return undefined;
}

function validateIpv6Cidr(value: string): string | undefined {
  if (!value) return "Required";
  if (!IPV6_CIDR_RE.test(value)) return "Enter a valid IPv6 CIDR (e.g. 2001:db8:1::/64)";
  return undefined;
}

function validateRangeOrder(start: string, end: string, ipRe: RegExp): string | undefined {
  if (!ipRe.test(start) || !ipRe.test(end)) return undefined;
  if (start > end) return "Range end must be greater than or equal to start";
  return undefined;
}

function ipv4ToNum(ip: string): number {
  const parts = ip.split(".").map(Number);
  return ((parts[0] << 24) | (parts[1] << 16) | (parts[2] << 8) | parts[3]) >>> 0;
}

function v4RangeCapacity(start: string, end: string): number {
  if (!IPV4_RE.test(start) || !IPV4_RE.test(end)) return 0;
  return ipv4ToNum(end) - ipv4ToNum(start) + 1;
}

function ipv6ToBigInt(ip: string): bigint {
  const full = ip.replace(/::/g, () => {
    const existing = ip.split(":").filter(Boolean).length;
    return ":" + "0:".repeat(8 - existing);
  }).replace(/^:|:$/g, "");
  const parts = full.split(":");
  let result = 0n;
  for (const part of parts) {
    result = (result << 16n) | BigInt(parseInt(part || "0", 16));
  }
  return result;
}

function v6RangeCapacity(start: string, end: string): number {
  if (!IPV6_RE.test(start) || !IPV6_RE.test(end)) return 0;
  const diff = ipv6ToBigInt(end) - ipv6ToBigInt(start) + 1n;
  return diff > BigInt(Number.MAX_SAFE_INTEGER) ? Number.MAX_SAFE_INTEGER : Number(diff);
}

function liveErr(value: string, validate: (v: string) => string | undefined, forceShow: boolean): string | undefined {
  if (!value && !forceShow) return undefined;
  return validate(value);
}

function V4PoolForm({
  form,
  onChange,
  showErrors,
}: {
  form: PoolFormState;
  onChange: (f: PoolFormState) => void;
  showErrors: boolean;
}) {
  const networkErr = liveErr(form.network, validateIpv4Cidr, showErrors);
  const startErr = liveErr(form.rangeStart, validateIpv4, showErrors);
  const endErr = liveErr(form.rangeEnd, (v) => validateIpv4(v) ?? validateRangeOrder(form.rangeStart, v, IPV4_RE), showErrors);

  return (
    <SpaceBetween size="m">
      <ColumnLayout columns={2}>
        <FormField label="Pool Name (optional)" description="A friendly label for quick search">
          <Input
            value={form.name}
            onChange={({ detail }) => onChange({ ...form, name: detail.value })}
            placeholder="e.g. Office LAN"
          />
        </FormField>
        <FormField label="Network (CIDR)" errorText={networkErr}>
          <Input
            value={form.network}
            onChange={({ detail }) => onChange({ ...form, network: detail.value })}
            placeholder="192.168.1.0/24"
            invalid={!!networkErr}
          />
        </FormField>
      </ColumnLayout>
      <ColumnLayout columns={2}>
        <FormField label="Range Start" errorText={startErr}>
          <Input
            value={form.rangeStart}
            onChange={({ detail }) => onChange({ ...form, rangeStart: detail.value })}
            placeholder="192.168.1.10"
            invalid={!!startErr}
          />
        </FormField>
        <FormField label="Range End" errorText={endErr}>
          <Input
            value={form.rangeEnd}
            onChange={({ detail }) => onChange({ ...form, rangeEnd: detail.value })}
            placeholder="192.168.1.250"
            invalid={!!endErr}
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
      <OptionsEditor
        options={form.options}
        onChange={(opts) => onChange({ ...form, options: opts })}
        family="v4"
      />
    </SpaceBetween>
  );
}

function V6PoolForm({
  form,
  onChange,
  showErrors,
}: {
  form: V6PoolFormState;
  onChange: (f: V6PoolFormState) => void;
  showErrors: boolean;
}) {
  const isRange = form.type === "range";
  const networkErr = liveErr(form.network, validateIpv6Cidr, showErrors);
  const startErr = isRange ? liveErr(form.rangeStart, validateIpv6, showErrors) : undefined;
  const endErr = isRange
    ? liveErr(form.rangeEnd, (v) => validateIpv6(v) ?? validateRangeOrder(form.rangeStart, v, IPV6_RE), showErrors)
    : undefined;
  const prefixErr = !isRange ? liveErr(form.prefix, validateIpv6Cidr, showErrors) : undefined;

  return (
    <SpaceBetween size="m">
      <FormField label="Pool Name (optional)" description="A friendly label for quick search">
        <Input
          value={form.name}
          onChange={({ detail }) => onChange({ ...form, name: detail.value })}
          placeholder="e.g. Server VLAN"
        />
      </FormField>
      <ColumnLayout columns={2}>
        <FormField label="Network (CIDR)" errorText={networkErr}>
          <Input
            value={form.network}
            onChange={({ detail }) => onChange({ ...form, network: detail.value })}
            placeholder="2001:db8:1::/64"
            invalid={!!networkErr}
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
          <FormField label="Range Start" errorText={startErr}>
            <Input
              value={form.rangeStart}
              onChange={({ detail }) => onChange({ ...form, rangeStart: detail.value })}
              placeholder="2001:db8:1::100"
              invalid={!!startErr}
            />
          </FormField>
          <FormField label="Range End" errorText={endErr}>
            <Input
              value={form.rangeEnd}
              onChange={({ detail }) => onChange({ ...form, rangeEnd: detail.value })}
              placeholder="2001:db8:1::1ff"
              invalid={!!endErr}
            />
          </FormField>
        </ColumnLayout>
      ) : (
        <ColumnLayout columns={2}>
          <FormField label="Prefix" errorText={prefixErr}>
            <Input
              value={form.prefix}
              onChange={({ detail }) => onChange({ ...form, prefix: detail.value })}
              placeholder="2001:db8:100::/56"
              invalid={!!prefixErr}
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
      <OptionsEditor
        options={form.options}
        onChange={(opts) => onChange({ ...form, options: opts })}
        family="v6"
      />
    </SpaceBetween>
  );
}

function UtilBar({ used, capacity }: { used: number; capacity: number }) {
  if (capacity <= 0) return <Box color="text-status-inactive">-</Box>;
  const pct = Math.min(100, Math.round((used / capacity) * 100));
  let color: "text-status-error" | "text-status-warning" | "text-status-success" = "text-status-success";
  if (pct >= 90) color = "text-status-error";
  else if (pct >= 70) color = "text-status-warning";
  return (
    <Box color={color} fontSize="body-s">
      {used} / {capacity.toLocaleString()} ({pct}%)
    </Box>
  );
}

function V4Pools({
  config,
  onSaved,
  leaseCounts,
}: {
  config: ConfigDocument;
  onSaved: () => void;
  leaseCounts: Record<string, number>;
}) {
  const allPools = extractV4Pools(config.document);
  const [filterText, setFilterText] = useState("");
  const pools = filterText
    ? allPools.filter((p) => {
        const q = filterText.toLowerCase();
        return p.name.toLowerCase().includes(q) || p.network.toLowerCase().includes(q);
      })
    : allPools;
  const [modalVisible, setModalVisible] = useState(false);
  const [form, setForm] = useState<PoolFormState>(EMPTY_V4_FORM);
  const [editNetwork, setEditNetwork] = useState<string | undefined>();
  const [editRangeIndex, setEditRangeIndex] = useState<number | undefined>();
  const [saving, setSaving] = useState(false);
  const [showErrors, setShowErrors] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const openAdd = () => {
    setForm(EMPTY_V4_FORM);
    setEditNetwork(undefined);
    setEditRangeIndex(undefined);
    setShowErrors(false);
    setModalVisible(true);
  };

  const openEdit = (row: PoolRow) => {
    setForm({
      name: row.name,
      network: row.network,
      rangeStart: row.rangeStart === "-" ? "" : row.rangeStart,
      rangeEnd: row.rangeEnd === "-" ? "" : row.rangeEnd,
      leaseDefault: row.leaseDefault === "-" ? "86400" : row.leaseDefault,
      leaseMin: row.leaseMin === "-" ? "1200" : row.leaseMin,
      leaseMax: row.leaseMax === "-" ? "604800" : row.leaseMax,
      serverId: row.serverId,
      probationPeriod: row.probationPeriod || "86400",
      pingCheck: false,
      options: row.options,
    });
    setEditNetwork(row.network);
    setEditRangeIndex(row.rangeIndex);
    setShowErrors(false);
    setModalVisible(true);
  };

  const hasErrors = () =>
    !!validateIpv4Cidr(form.network) ||
    !!validateIpv4(form.rangeStart) ||
    !!validateIpv4(form.rangeEnd) ||
    !!validateRangeOrder(form.rangeStart, form.rangeEnd, IPV4_RE);

  const save = async () => {
    setShowErrors(true);
    if (hasErrors()) return;
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
        filter={
          <TextFilter
            filteringText={filterText}
            onChange={({ detail }) => setFilterText(detail.filteringText)}
            filteringPlaceholder="Filter by name or network"
            countText={`${pools.length} match${pools.length !== 1 ? "es" : ""}`}
          />
        }
        columnDefinitions={[
          { id: "name", header: "Name", cell: (r) => r.name || "-", width: 150 },
          { id: "network", header: "Network", cell: (r) => r.network, width: 180 },
          { id: "start", header: "Range Start", cell: (r) => r.rangeStart, width: 160 },
          { id: "end", header: "Range End", cell: (r) => r.rangeEnd, width: 160 },
          { id: "lease", header: "Lease (def/min/max)", cell: (r) => `${r.leaseDefault} / ${r.leaseMin} / ${r.leaseMax}`, width: 200 },
          {
            id: "util",
            header: "Usage",
            cell: (r) => {
              const capacity = v4RangeCapacity(r.rangeStart, r.rangeEnd);
              const used = leaseCounts[r.network] ?? 0;
              return <UtilBar used={used} capacity={capacity} />;
            },
            width: 140,
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
              <Button variant="primary" loading={saving} onClick={save}>
                {editNetwork != null ? "Save changes" : "Add pool"}
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <V4PoolForm form={form} onChange={setForm} showErrors={showErrors} />
      </Modal>
    </SpaceBetween>
  );
}

function V6Pools({
  config,
  onSaved,
  leaseCounts,
}: {
  config: ConfigDocument;
  onSaved: () => void;
  leaseCounts: Record<string, number>;
}) {
  const allPools = extractV6Pools(config.document);
  const [filterText, setFilterText] = useState("");
  const pools = filterText
    ? allPools.filter((p) => {
        const q = filterText.toLowerCase();
        return p.name.toLowerCase().includes(q) || p.network.toLowerCase().includes(q);
      })
    : allPools;
  const [modalVisible, setModalVisible] = useState(false);
  const [form, setForm] = useState<V6PoolFormState>(EMPTY_V6_FORM);
  const [editNetwork, setEditNetwork] = useState<string | undefined>();
  const [editRangeIndex, setEditRangeIndex] = useState<number | undefined>();
  const [editType, setEditType] = useState<"range" | "pd_pool" | undefined>();
  const [saving, setSaving] = useState(false);
  const [showErrors, setShowErrors] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);

  const openAdd = () => {
    setForm(EMPTY_V6_FORM);
    setEditNetwork(undefined);
    setEditRangeIndex(undefined);
    setEditType(undefined);
    setShowErrors(false);
    setModalVisible(true);
  };

  const openEdit = (row: V6PoolRow) => {
    setForm({
      name: row.name,
      network: row.network,
      type: row.type,
      rangeStart: row.rangeStart === "-" ? "" : row.rangeStart,
      rangeEnd: row.rangeEnd === "-" ? "" : row.rangeEnd,
      prefix: row.prefix,
      delegatedLen: row.delegatedLen || "64",
      leaseDefault: row.leaseDefault === "-" ? "3600" : row.leaseDefault,
      preferredDefault: row.preferredDefault === "-" ? "3600" : row.preferredDefault,
      options: row.options,
    });
    setEditNetwork(row.network);
    setEditRangeIndex(row.rangeIndex);
    setEditType(row.type);
    setShowErrors(false);
    setModalVisible(true);
  };

  const hasErrors = () => {
    if (validateIpv6Cidr(form.network)) return true;
    if (form.type === "range") {
      return !!validateIpv6(form.rangeStart) || !!validateIpv6(form.rangeEnd)
        || !!validateRangeOrder(form.rangeStart, form.rangeEnd, IPV6_RE);
    }
    return !!validateIpv6Cidr(form.prefix);
  };

  const save = async () => {
    setShowErrors(true);
    if (hasErrors()) return;
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
        filter={
          <TextFilter
            filteringText={filterText}
            onChange={({ detail }) => setFilterText(detail.filteringText)}
            filteringPlaceholder="Filter by name or network"
            countText={`${pools.length} match${pools.length !== 1 ? "es" : ""}`}
          />
        }
        columnDefinitions={[
          { id: "name", header: "Name", cell: (r) => r.name || "-", width: 150 },
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
            id: "util",
            header: "Usage",
            cell: (r) => {
              if (r.type === "pd_pool") return <Box color="text-status-inactive">-</Box>;
              const capacity = v6RangeCapacity(r.rangeStart, r.rangeEnd);
              const used = leaseCounts[r.network] ?? 0;
              return <UtilBar used={used} capacity={capacity} />;
            },
            width: 140,
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
              <Button variant="primary" loading={saving} onClick={save}>
                {editNetwork != null ? "Save changes" : "Add pool"}
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <V6PoolForm form={form} onChange={setForm} showErrors={showErrors} />
      </Modal>
    </SpaceBetween>
  );
}

function buildLeaseCounts(
  items: { network: string }[]
): Record<string, number> {
  const counts: Record<string, number> = {};
  for (const item of items) {
    counts[item.network] = (counts[item.network] ?? 0) + 1;
  }
  return counts;
}

export default function Pools() {
  const [config, setConfig] = useState<ConfigDocument | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [v4LeaseCounts, setV4LeaseCounts] = useState<Record<string, number>>({});
  const [v6LeaseCounts, setV6LeaseCounts] = useState<Record<string, number>>({});

  const load = () => {
    setLoading(true);
    setError(null);
    Promise.all([
      api.config(),
      api.leasesV4({ limit: "10000" }).catch(() => ({ items: [] })),
      api.leasesV6({ limit: "10000" }).catch(() => ({ items: [] })),
    ])
      .then(([cfg, v4, v6]) => {
        setConfig(cfg);
        setV4LeaseCounts(buildLeaseCounts(v4.items));
        setV6LeaseCounts(buildLeaseCounts(v6.items));
      })
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
                content: <V4Pools config={config} onSaved={load} leaseCounts={v4LeaseCounts} />,
              },
              {
                id: "v6",
                label: "DHCPv6",
                content: <V6Pools config={config} onSaved={load} leaseCounts={v6LeaseCounts} />,
              },
            ]}
          />
        )}
      </SpaceBetween>
    </ContentLayout>
  );
}
