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

interface DiffLine {
  type: "add" | "remove" | "context";
  text: string;
}

function lcs(a: string[], b: string[]): Set<number> {
  const m = a.length;
  const n = b.length;
  const dp: number[][] = Array.from({ length: m + 1 }, () => Array(n + 1).fill(0));
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= n; j++) {
      dp[i][j] = a[i - 1] === b[j - 1] ? dp[i - 1][j - 1] + 1 : Math.max(dp[i - 1][j], dp[i][j - 1]);
    }
  }
  const result: number[] = [];
  let i = m, j = n;
  while (i > 0 && j > 0) {
    if (a[i - 1] === b[j - 1]) {
      result.push(i - 1);
      i--;
      j--;
    } else if (dp[i - 1][j] >= dp[i][j - 1]) {
      i--;
    } else {
      j--;
    }
  }
  return new Set(result);
}

function buildRawDiff(oldLines: string[], newLines: string[], commonOld: Set<number>): DiffLine[] {
  const rawDiff: DiffLine[] = [];
  let oi = 0, ni = 0;
  const sorted = [...commonOld].sort((a, b) => a - b);
  let ci = 0;

  while (oi < oldLines.length || ni < newLines.length) {
    if (ci < sorted.length && oi === sorted[ci]) {
      rawDiff.push({ type: "context", text: oldLines[oi] });
      oi++;
      ni++;
      ci++;
    } else if (oi < oldLines.length && (ci >= sorted.length || oi < sorted[ci])) {
      rawDiff.push({ type: "remove", text: oldLines[oi] });
      oi++;
    } else {
      rawDiff.push({ type: "add", text: newLines[ni] });
      ni++;
    }
  }
  return rawDiff;
}

function collapseContext(rawDiff: DiffLine[], contextLines: number): DiffLine[] {
  const changed = new Set<number>();
  rawDiff.forEach((line, idx) => {
    if (line.type !== "context") changed.add(idx);
  });

  const visible = new Set<number>();
  for (const idx of changed) {
    for (let c = Math.max(0, idx - contextLines); c <= Math.min(rawDiff.length - 1, idx + contextLines); c++) {
      visible.add(c);
    }
  }
  if (visible.size === 0) return [];

  const result: DiffLine[] = [];
  let lastIdx = -2;
  for (let idx = 0; idx < rawDiff.length; idx++) {
    if (!visible.has(idx)) continue;
    if (lastIdx >= 0 && idx - lastIdx > 1) {
      result.push({ type: "context", text: "···" });
    }
    result.push(rawDiff[idx]);
    lastIdx = idx;
  }
  return result;
}

function unifiedDiff(oldText: string, newText: string, contextLines = 3): DiffLine[] {
  const oldLines = oldText.split("\n");
  const newLines = newText.split("\n");
  const rawDiff = buildRawDiff(oldLines, newLines, lcs(oldLines, newLines));
  return collapseContext(rawDiff, contextLines);
}

function UnifiedDiffView({ oldDoc, newDoc }: { oldDoc: Record<string, unknown>; newDoc: Record<string, unknown> }) {
  const oldText = JSON.stringify(oldDoc, null, 2);
  const newText = JSON.stringify(newDoc, null, 2);
  const lines = unifiedDiff(oldText, newText);

  if (lines.length === 0) {
    return <Box color="text-status-inactive">No differences — candidate matches the active configuration.</Box>;
  }

  return (
    <pre style={{
      margin: 0,
      padding: 0,
      fontSize: "13px",
      fontFamily: "'SF Mono', 'Cascadia Code', 'Fira Code', Consolas, monospace",
      lineHeight: 1.6,
      borderRadius: "6px",
      overflow: "auto",
      border: "1px solid var(--color-border-divider-default, #414d5c)",
    }}>
      {lines.map((line, i) => {
        let bg: string;
        let color: string;
        let prefix: string;
        if (line.type === "add") {
          bg = "rgba(35, 134, 54, 0.15)";
          color = "#3fb950";
          prefix = "+";
        } else if (line.type === "remove") {
          bg = "rgba(248, 81, 73, 0.15)";
          color = "#f85149";
          prefix = "-";
        } else {
          bg = "transparent";
          color = "var(--color-text-body-default, #d1d5db)";
          prefix = " ";
        }
        return (
          <div key={i} style={{
            backgroundColor: bg,
            padding: "0 12px",
            whiteSpace: "pre",
            minHeight: "1.6em",
          }}>
            <span style={{ color: "#6e7681", userSelect: "none", display: "inline-block", width: "2ch", marginRight: "8px" }}>
              {prefix}
            </span>
            <span style={{ color }}>{line.text}</span>
          </div>
        );
      })}
    </pre>
  );
}

function alertType(level: string): "error" | "warning" | "info" {
  if (level === "error") return "error";
  if (level === "warning") return "warning";
  return "info";
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
                      <Alert key={i} type={alertType(v.level)}>
                        {v.path && <Box variant="code">{v.path}</Box>}
                        {v.message}
                      </Alert>
                    ))}
                  </SpaceBetween>
                )}
                {activeConfig?.document && candidate.document ? (
                  <UnifiedDiffView oldDoc={activeConfig.document} newDoc={candidate.document} />
                ) : (
                  <Box color="text-status-inactive">No document available.</Box>
                )}
              </SpaceBetween>
            </Container>
          );
        })}
      </SpaceBetween>
    </Modal>
  );
}
