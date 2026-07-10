import { useEffect, useState, useCallback } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Modal from "@cloudscape-design/components/modal";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Alert from "@cloudscape-design/components/alert";
import Spinner from "@cloudscape-design/components/spinner";
import Badge from "@cloudscape-design/components/badge";
import Container from "@cloudscape-design/components/container";
import Header from "@cloudscape-design/components/header";
import { api, ConfigCandidate, ConfigDocument } from "../api";

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

interface DiffEntry {
  path: string;
  type: "added" | "removed" | "changed";
  oldVal?: string;
  newVal?: string;
}

function jsonDiff(a: unknown, b: unknown, path = ""): DiffEntry[] {
  if (a === b) return [];
  const strA = JSON.stringify(a, null, 2);
  const strB = JSON.stringify(b, null, 2);
  if (strA === strB) return [];

  const isObjA = a !== null && typeof a === "object" && !Array.isArray(a);
  const isObjB = b !== null && typeof b === "object" && !Array.isArray(b);

  if (isObjA && isObjB) {
    const diffs: DiffEntry[] = [];
    const objA = a as Record<string, unknown>;
    const objB = b as Record<string, unknown>;
    const allKeys = new Set([...Object.keys(objA), ...Object.keys(objB)]);
    for (const key of allKeys) {
      const childPath = path ? `${path}.${key}` : key;
      if (key in objA && !(key in objB)) {
        diffs.push({ path: childPath, type: "removed", oldVal: JSON.stringify(objA[key], null, 2) });
      } else if (!(key in objA) && key in objB) {
        diffs.push({ path: childPath, type: "added", newVal: JSON.stringify(objB[key], null, 2) });
      } else {
        diffs.push(...jsonDiff(objA[key], objB[key], childPath));
      }
    }
    return diffs;
  }

  return [{ path, type: "changed", oldVal: strA, newVal: strB }];
}

function statusColor(status: ConfigCandidate["status"]): "blue" | "green" | "red" | "grey" {
  if (status === "valid" || status === "activated") return "green";
  if (status === "invalid") return "red";
  if (status === "staged" || status === "validating") return "blue";
  return "grey";
}

export function usePendingCount() {
  const [count, setCount] = useState(0);

  const refresh = useCallback(() => {
    api.configCandidates()
      .then((res) => {
        const pending = res.items.filter(
          (c) => c.status === "staged" || c.status === "valid" || c.status === "validating"
        );
        setCount(pending.length);
      })
      .catch(() => setCount(0));
  }, []);

  useEffect(() => {
    refresh();
    const id = setInterval(refresh, 15000);
    return () => clearInterval(id);
  }, [refresh]);

  return { count, refresh };
}

export default function PendingChanges({
  visible,
  onDismiss,
  onActivated,
}: {
  visible: boolean;
  onDismiss: () => void;
  onActivated: () => void;
}) {
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [success, setSuccess] = useState<string | null>(null);
  const [activeConfig, setActiveConfig] = useState<ConfigDocument | null>(null);
  const [candidates, setCandidates] = useState<ConfigCandidate[]>([]);
  const [activating, setActivating] = useState(false);

  useEffect(() => {
    if (!visible) return;
    setLoading(true);
    setError(null);
    setSuccess(null);
    Promise.all([api.config(), api.configCandidates()])
      .then(async ([cfg, list]) => {
        setActiveConfig(cfg);
        const pending = list.items.filter(
          (c) => c.status === "staged" || c.status === "valid" || c.status === "validating"
        );
        const detailed = await Promise.all(
          pending.map((c) => api.configCandidate(c.candidate_id))
        );
        setCandidates(detailed);
      })
      .catch((err) => setError(String(err)))
      .finally(() => setLoading(false));
  }, [visible]);

  const activate = async (candidateId: string) => {
    setActivating(true);
    setError(null);
    try {
      await api.activateConfig(candidateId);
      setSuccess("Configuration activated successfully.");
      setCandidates((prev) => prev.filter((c) => c.candidate_id !== candidateId));
      onActivated();
    } catch (err) {
      setError(String(err));
    } finally {
      setActivating(false);
    }
  };

  return (
    <Modal
      visible={visible}
      onDismiss={onDismiss}
      header="Pending Changes"
      size="max"
      footer={
        <Box float="right">
          <Button variant="link" onClick={onDismiss}>Close</Button>
        </Box>
      }
    >
      <SpaceBetween size="m">
        {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}
        {success && <Alert type="success" dismissible onDismiss={() => setSuccess(null)}>{success}</Alert>}
        {loading && <Spinner size="large" />}
        {!loading && candidates.length === 0 && !error && (
          <Box textAlign="center" padding="l" color="text-status-inactive">
            No pending changes.
          </Box>
        )}
        {candidates.map((candidate) => {
          const diffs = activeConfig?.document && candidate.document
            ? jsonDiff(activeConfig.document, candidate.document)
            : [];
          const canActivate = candidate.status === "valid" || candidate.status === "staged";

          return (
            <Container
              key={candidate.candidate_id}
              header={
                <Header
                  actions={
                    <SpaceBetween direction="horizontal" size="xs">
                      <Badge color={statusColor(candidate.status)}>{candidate.status}</Badge>
                      {canActivate && (
                        <Button variant="primary" loading={activating} onClick={() => activate(candidate.candidate_id)}>
                          Activate
                        </Button>
                      )}
                    </SpaceBetween>
                  }
                >
                  Candidate {candidate.candidate_id.slice(0, 8)}…
                  <Box variant="small" display="inline-block" margin={{ left: "s" }}>
                    {new Date(candidate.created_at).toLocaleString()}
                  </Box>
                </Header>
              }
            >
              <SpaceBetween size="s">
                {candidate.validation && candidate.validation.length > 0 && (
                  <SpaceBetween size="xs">
                    {candidate.validation.map((v, i) => (
                      <Alert key={i} type={v.level === "error" ? "error" : v.level === "warning" ? "warning" : "info"}>
                        {v.path && <Box variant="code">{v.path}</Box>}
                        {v.message}
                      </Alert>
                    ))}
                  </SpaceBetween>
                )}
                {diffs.length === 0 ? (
                  <Box color="text-status-inactive">No differences detected.</Box>
                ) : (
                  <SpaceBetween size="xs">
                    <Box variant="small">{diffs.length} change{diffs.length !== 1 ? "s" : ""}</Box>
                    {diffs.map((d, i) => (
                      <div key={i}>
                        <Box variant="code" fontSize="body-s">
                          <Badge color={d.type === "added" ? "green" : d.type === "removed" ? "red" : "blue"}>
                            {d.type}
                          </Badge>{" "}
                          {d.path}
                        </Box>
                        {d.oldVal !== undefined && (
                          <pre style={{ ...CODE_STYLE, borderLeft: "3px solid #d13212" }}>{d.oldVal}</pre>
                        )}
                        {d.newVal !== undefined && (
                          <pre style={{ ...CODE_STYLE, borderLeft: "3px solid #1d8102" }}>{d.newVal}</pre>
                        )}
                      </div>
                    ))}
                  </SpaceBetween>
                )}
              </SpaceBetween>
            </Container>
          );
        })}
      </SpaceBetween>
    </Modal>
  );
}
