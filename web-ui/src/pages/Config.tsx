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
import Select from "@cloudscape-design/components/select";
import Tabs from "@cloudscape-design/components/tabs";
import Badge from "@cloudscape-design/components/badge";
import Modal from "@cloudscape-design/components/modal";
import Textarea from "@cloudscape-design/components/textarea";
import { api, post, ConfigDocument, ConfigCandidate } from "../api";
import { useNotifications } from "../components/Notifications";

const CODE_STYLE: React.CSSProperties = {
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
};

function statusColor(status: ConfigCandidate["status"]): "blue" | "green" | "red" | "grey" {
  if (status === "valid" || status === "activated") return "green";
  if (status === "invalid") return "red";
  if (status === "staged" || status === "validating") return "blue";
  return "grey";
}

function jsonDiff(
  a: Record<string, unknown>,
  b: Record<string, unknown>
): { path: string; type: "added" | "removed" | "changed"; oldVal?: string; newVal?: string }[] {
  const diffs: { path: string; type: "added" | "removed" | "changed"; oldVal?: string; newVal?: string }[] = [];

  function walk(objA: unknown, objB: unknown, path: string) {
    if (objA === objB) return;
    const strA = JSON.stringify(objA, null, 2);
    const strB = JSON.stringify(objB, null, 2);
    if (strA === strB) return;

    const isObjA = objA !== null && typeof objA === "object" && !Array.isArray(objA);
    const isObjB = objB !== null && typeof objB === "object" && !Array.isArray(objB);

    if (isObjA && isObjB) {
      const keysA = Object.keys(objA as Record<string, unknown>);
      const keysB = Object.keys(objB as Record<string, unknown>);
      const allKeys = new Set([...keysA, ...keysB]);
      for (const key of allKeys) {
        const inA = key in (objA as Record<string, unknown>);
        const inB = key in (objB as Record<string, unknown>);
        const childPath = path ? `${path}.${key}` : key;
        if (inA && !inB) {
          diffs.push({ path: childPath, type: "removed", oldVal: JSON.stringify((objA as Record<string, unknown>)[key], null, 2) });
        } else if (!inA && inB) {
          diffs.push({ path: childPath, type: "added", newVal: JSON.stringify((objB as Record<string, unknown>)[key], null, 2) });
        } else {
          walk((objA as Record<string, unknown>)[key], (objB as Record<string, unknown>)[key], childPath);
        }
      }
    } else {
      diffs.push({ path, type: "changed", oldVal: strA, newVal: strB });
    }
  }

  walk(a, b, "");
  return diffs;
}

function DiffView({ active, candidate }: { active: Record<string, unknown>; candidate: Record<string, unknown> }) {
  const diffs = jsonDiff(active, candidate);

  if (diffs.length === 0) {
    return (
      <Box textAlign="center" padding="l" color="text-status-info">
        No differences — candidate matches the active configuration.
      </Box>
    );
  }

  return (
    <SpaceBetween size="s">
      <Box variant="small">{diffs.length} change{diffs.length !== 1 ? "s" : ""} detected</Box>
      {diffs.map((d, i) => (
        <Container key={i} variant="stacked">
          <SpaceBetween size="xxs">
            <Box variant="code" fontSize="body-s">
              <Badge color={d.type === "added" ? "green" : d.type === "removed" ? "red" : "blue"}>
                {d.type}
              </Badge>
              {" "}
              {d.path}
            </Box>
            {d.oldVal !== undefined && (
              <pre style={{ ...CODE_STYLE, borderLeft: "3px solid #d13212" }}>
                {d.oldVal}
              </pre>
            )}
            {d.newVal !== undefined && (
              <pre style={{ ...CODE_STYLE, borderLeft: "3px solid #1d8102" }}>
                {d.newVal}
              </pre>
            )}
          </SpaceBetween>
        </Container>
      ))}
    </SpaceBetween>
  );
}

function ConfigEditor({
  config,
  onSaved,
}: {
  config: ConfigDocument;
  onSaved: () => void;
}) {
  const { notify } = useNotifications();
  const [value, setValue] = useState(() => JSON.stringify(config.document, null, 2));
  const [parseError, setParseError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [confirmVisible, setConfirmVisible] = useState(false);
  const [diffCount, setDiffCount] = useState(0);

  const validate = (): Record<string, unknown> | null => {
    try {
      const parsed = JSON.parse(value);
      if (typeof parsed !== "object" || parsed === null || Array.isArray(parsed)) {
        setParseError("Config must be a JSON object");
        return null;
      }
      setParseError(null);
      return parsed;
    } catch (e) {
      const msg = e instanceof SyntaxError ? e.message : "Invalid JSON";
      setParseError(msg);
      return null;
    }
  };

  const handleStage = () => {
    const parsed = validate();
    if (!parsed) return;
    const diffs = jsonDiff(config.document, parsed);
    if (diffs.length === 0) {
      setParseError("No changes detected");
      return;
    }
    setDiffCount(diffs.length);
    setConfirmVisible(true);
  };

  const submitCandidate = async () => {
    const parsed = JSON.parse(value);
    setSaving(true);
    setError(null);
    try {
      await post("/v1/config/candidates", { document: parsed });
      notify("success", "Configuration candidate staged. Commit from the top nav.");
      setConfirmVisible(false);
      onSaved();
    } catch (err) {
      setError(String(err));
      setConfirmVisible(false);
    } finally {
      setSaving(false);
    }
  };

  const handleReset = () => {
    setValue(JSON.stringify(config.document, null, 2));
    setParseError(null);
    setError(null);
  };

  const handleFormat = () => {
    try {
      const parsed = JSON.parse(value);
      setValue(JSON.stringify(parsed, null, 2));
      setParseError(null);
    } catch (e) {
      const msg = e instanceof SyntaxError ? e.message : "Invalid JSON";
      setParseError(msg);
    }
  };

  const lineCount = value.split("\n").length;

  return (
    <SpaceBetween size="m">
      {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
      {parseError && <Alert type="error">{parseError}</Alert>}

      <div style={{ position: "relative" }}>
        <Textarea
          value={value}
          onChange={({ detail }) => {
            setValue(detail.value);
            if (parseError) {
              try {
                JSON.parse(detail.value);
                setParseError(null);
              } catch {
                // don't clear error until it's fixed
              }
            }
          }}
          rows={Math.min(40, Math.max(20, lineCount + 2))}
          spellcheck={false}
        />
        <Box fontSize="body-s" color="text-status-inactive" padding={{ top: "xxs" }}>
          {lineCount} lines
        </Box>
      </div>

      <SpaceBetween direction="horizontal" size="xs">
        <Button onClick={handleFormat}>Format</Button>
        <Button onClick={handleReset}>Reset</Button>
        <Button variant="primary" onClick={handleStage}>
          Stage changes
        </Button>
      </SpaceBetween>

      <Modal
        visible={confirmVisible}
        onDismiss={() => setConfirmVisible(false)}
        header="Stage Configuration Candidate"
        footer={
          <Box float="right">
            <SpaceBetween direction="horizontal" size="xs">
              <Button variant="link" onClick={() => setConfirmVisible(false)}>Cancel</Button>
              <Button variant="primary" loading={saving} onClick={submitCandidate}>
                Stage candidate
              </Button>
            </SpaceBetween>
          </Box>
        }
      >
        <SpaceBetween size="s">
          <Box>
            This will create a new configuration candidate with{" "}
            <strong>{diffCount} change{diffCount !== 1 ? "s" : ""}</strong>.
          </Box>
          <Box>
            After staging, use the <strong>Commit</strong> button in the top navigation
            to validate and activate.
          </Box>
        </SpaceBetween>
      </Modal>
    </SpaceBetween>
  );
}

export default function Config() {
  const [config, setConfig] = useState<ConfigDocument | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [candidates, setCandidates] = useState<ConfigCandidate[]>([]);
  const [selectedCandidate, setSelectedCandidate] = useState<ConfigCandidate | null>(null);
  const [candidateLoading, setCandidateLoading] = useState(false);

  const load = () => {
    setLoading(true);
    setError(null);
    Promise.all([api.config(), api.configCandidates()])
      .then(([cfg, cands]) => {
        setConfig(cfg);
        setCandidates(cands.items);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  };

  useEffect(() => {
    load();
  }, []);

  const loadCandidate = (id: string) => {
    setCandidateLoading(true);
    api
      .configCandidate(id)
      .then(setSelectedCandidate)
      .catch((err) => setError(String(err)))
      .finally(() => setCandidateLoading(false));
  };

  if (loading && !config) {
    return <Spinner size="large" />;
  }

  return (
    <ContentLayout
      header={
        <Header
          variant="h1"
          description="View and edit server configuration"
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
              <Tabs
                tabs={[
                  {
                    id: "active",
                    label: "Active Config",
                    content: (
                      <Box>
                        <pre style={CODE_STYLE}>
                          {JSON.stringify(config.document, null, 2)}
                        </pre>
                      </Box>
                    ),
                  },
                  {
                    id: "editor",
                    label: "Editor",
                    content: <ConfigEditor config={config} onSaved={load} />,
                  },
                  {
                    id: "diff",
                    label: `Diff View${candidates.length > 0 ? ` (${candidates.length})` : ""}`,
                    content: (
                      <SpaceBetween size="m">
                        {candidates.length === 0 ? (
                          <Box textAlign="center" padding="l" color="inherit">
                            No staged candidates to compare.
                          </Box>
                        ) : (
                          <>
                            <ColumnLayout columns={2}>
                              <Select
                                placeholder="Select a candidate..."
                                selectedOption={
                                  selectedCandidate
                                    ? {
                                        label: `${selectedCandidate.candidate_id.slice(0, 8)}… — ${selectedCandidate.status} (${new Date(selectedCandidate.created_at).toLocaleString()})`,
                                        value: selectedCandidate.candidate_id,
                                      }
                                    : null
                                }
                                onChange={({ detail }) => {
                                  if (detail.selectedOption.value) {
                                    loadCandidate(detail.selectedOption.value);
                                  }
                                }}
                                options={candidates.map((c) => ({
                                  label: `${c.candidate_id.slice(0, 8)}… — ${c.status} (${new Date(c.created_at).toLocaleString()})`,
                                  value: c.candidate_id,
                                  tags: [c.status],
                                }))}
                              />
                              <div>
                                {selectedCandidate && (
                                  <Badge color={statusColor(selectedCandidate.status)}>
                                    {selectedCandidate.status}
                                  </Badge>
                                )}
                              </div>
                            </ColumnLayout>
                            {candidateLoading && <Spinner />}
                            {selectedCandidate?.document && config && (
                              <DiffView
                                active={config.document}
                                candidate={selectedCandidate.document}
                              />
                            )}
                            {selectedCandidate?.validation && selectedCandidate.validation.length > 0 && (
                              <Container header={<Header variant="h3">Validation</Header>}>
                                <SpaceBetween size="xs">
                                  {selectedCandidate.validation.map((v, i) => (
                                    <Alert key={i} type={v.level === "error" ? "error" : v.level === "warning" ? "warning" : "info"}>
                                      {v.path && <Box variant="code">{v.path}</Box>}
                                      {v.message}
                                    </Alert>
                                  ))}
                                </SpaceBetween>
                              </Container>
                            )}
                          </>
                        )}
                      </SpaceBetween>
                    ),
                  },
                ]}
              />
            </Container>
          </>
        )}
      </SpaceBetween>
    </ContentLayout>
  );
}
