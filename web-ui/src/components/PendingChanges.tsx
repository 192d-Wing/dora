import { useEffect, useState, useCallback, useRef } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Modal from "@cloudscape-design/components/modal";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Alert from "@cloudscape-design/components/alert";
import Spinner from "@cloudscape-design/components/spinner";
import StatusIndicator from "@cloudscape-design/components/status-indicator";
import { api, ConfigCandidate, ConfigDocument } from "../api";

interface DiffLine {
  type: "add" | "remove" | "context";
  text: string;
}

function computeRawDiff(oldLines: string[], newLines: string[]): DiffLine[] {
  const m = oldLines.length;
  const n = newLines.length;
  const dp: number[][] = Array.from({ length: m + 1 }, () => new Array(n + 1).fill(0));
  for (let i = 1; i <= m; i++) {
    for (let j = 1; j <= n; j++) {
      dp[i][j] = oldLines[i - 1] === newLines[j - 1]
        ? dp[i - 1][j - 1] + 1
        : Math.max(dp[i - 1][j], dp[i][j - 1]);
    }
  }

  const result: DiffLine[] = [];
  let i = m, j = n;
  while (i > 0 || j > 0) {
    if (i > 0 && j > 0 && oldLines[i - 1] === newLines[j - 1]) {
      result.push({ type: "context", text: oldLines[i - 1] });
      i--;
      j--;
    } else if (j > 0 && (i === 0 || dp[i][j - 1] >= dp[i - 1][j])) {
      result.push({ type: "add", text: newLines[j - 1] });
      j--;
    } else {
      result.push({ type: "remove", text: oldLines[i - 1] });
      i--;
    }
  }
  result.reverse();
  return result;
}

function unifiedDiff(oldText: string, newText: string, contextLines = 3): DiffLine[] {
  const raw = computeRawDiff(oldText.split("\n"), newText.split("\n"));

  const changed = new Set<number>();
  raw.forEach((line, idx) => {
    if (line.type !== "context") changed.add(idx);
  });
  if (changed.size === 0) return [];

  const visible = new Set<number>();
  for (const idx of changed) {
    for (let c = Math.max(0, idx - contextLines); c <= Math.min(raw.length - 1, idx + contextLines); c++) {
      visible.add(c);
    }
  }

  const result: DiffLine[] = [];
  let lastIdx = -2;
  for (let idx = 0; idx < raw.length; idx++) {
    if (!visible.has(idx)) continue;
    if (lastIdx >= 0 && idx - lastIdx > 1) {
      result.push({ type: "context", text: "···" });
    }
    result.push(raw[idx]);
    lastIdx = idx;
  }
  return result;
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

type CommitPhase = "loading" | "preview" | "validating" | "valid" | "invalid" | "committing" | "done";

export default function PendingChanges({
  visible,
  onDismiss,
  onActivated,
}: {
  visible: boolean;
  onDismiss: () => void;
  onActivated: () => void;
}) {
  const [phase, setPhase] = useState<CommitPhase>("loading");
  const [error, setError] = useState<string | null>(null);
  const [activeConfig, setActiveConfig] = useState<ConfigDocument | null>(null);
  const [candidate, setCandidate] = useState<ConfigCandidate | null>(null);
  const pollRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const stopPolling = useCallback(() => {
    if (pollRef.current) {
      clearInterval(pollRef.current);
      pollRef.current = null;
    }
  }, []);

  const loadCandidate = useCallback(() => {
    setPhase("loading");
    setError(null);
    Promise.all([api.config(), api.configCandidates()])
      .then(async ([cfg, list]) => {
        setActiveConfig(cfg);
        const pending = list.items.filter(
          (c) => c.status === "staged" || c.status === "valid" || c.status === "validating"
        );
        if (pending.length === 0) {
          setCandidate(null);
          setPhase("preview");
          return;
        }
        const latest = await api.configCandidate(pending[0].candidate_id);
        setCandidate(latest);
        if (latest.status === "valid") {
          setPhase("valid");
        } else if (latest.status === "invalid") {
          setPhase("invalid");
        } else {
          setPhase("preview");
        }
      })
      .catch((err) => {
        setError(String(err));
        setPhase("preview");
      });
  }, []);

  useEffect(() => {
    if (visible) {
      loadCandidate();
    } else {
      stopPolling();
    }
    return stopPolling;
  }, [visible, loadCandidate, stopPolling]);

  const validate = () => {
    if (!candidate) return;
    setPhase("validating");
    setError(null);

    const poll = () => {
      api.configCandidate(candidate.candidate_id)
        .then((c) => {
          setCandidate(c);
          if (c.status === "valid") {
            stopPolling();
            setPhase("valid");
          } else if (c.status === "invalid") {
            stopPolling();
            setPhase("invalid");
          }
        })
        .catch((err) => {
          stopPolling();
          setError(String(err));
          setPhase("preview");
        });
    };

    poll();
    pollRef.current = setInterval(poll, 2000);
  };

  const commit = async () => {
    if (!candidate) return;
    setPhase("committing");
    setError(null);
    try {
      await api.activateConfig(candidate.candidate_id);
      setPhase("done");
      onActivated();
    } catch (err) {
      setError(String(err));
      setPhase("valid");
    }
  };

  const hasValidationErrors = candidate?.validation && candidate.validation.length > 0;
  const hasDiff = activeConfig?.document && candidate?.document;

  return (
    <Modal
      visible={visible}
      onDismiss={onDismiss}
      header={
        <SpaceBetween direction="horizontal" size="xs">
          <span>Commit</span>
          {candidate && (
            <Box fontSize="body-s" color="text-status-inactive" display="inline-block" padding={{ top: "xxs" }}>
              {candidate.candidate_id.slice(0, 8)}…
            </Box>
          )}
        </SpaceBetween>
      }
      size="max"
      footer={
        <Box float="right">
          <SpaceBetween direction="horizontal" size="xs">
            {phase === "done" ? (
              <Button variant="primary" onClick={onDismiss}>Done</Button>
            ) : (
              <>
                <Button variant="link" onClick={onDismiss}>Cancel</Button>
                {candidate && phase === "preview" && (
                  <Button onClick={validate}>
                    Validate
                  </Button>
                )}
                {candidate && phase === "valid" && (
                  <>
                    <Button onClick={validate}>Re-validate</Button>
                    <Button variant="primary" onClick={commit}>Commit</Button>
                  </>
                )}
                {candidate && phase === "invalid" && (
                  <Button onClick={validate}>Re-validate</Button>
                )}
              </>
            )}
          </SpaceBetween>
        </Box>
      }
    >
      <SpaceBetween size="m">
        {error && <Alert type="error" dismissible onDismiss={() => setError(null)}>{error}</Alert>}

        {phase === "done" && (
          <Alert type="success">Configuration committed and activated successfully.</Alert>
        )}

        {phase === "loading" && (
          <Box textAlign="center" padding="xl"><Spinner size="large" /></Box>
        )}

        {phase !== "loading" && !candidate && phase !== "done" && (
          <Box textAlign="center" padding="l" color="text-status-inactive">
            No pending changes to commit.
          </Box>
        )}

        {candidate && phase !== "loading" && (
          <SpaceBetween size="m">
            <div style={{
              display: "flex",
              alignItems: "center",
              gap: "16px",
              padding: "12px 16px",
              borderRadius: "8px",
              backgroundColor: "var(--color-background-layout-toggle-default, #1a2332)",
              border: "1px solid var(--color-border-divider-default, #414d5c)",
            }}>
              {phase === "preview" && (
                <StatusIndicator type="pending">Not validated — click Validate to check</StatusIndicator>
              )}
              {phase === "validating" && (
                <StatusIndicator type="loading">Validating configuration…</StatusIndicator>
              )}
              {phase === "valid" && (
                <StatusIndicator type="success">Validation passed — ready to commit</StatusIndicator>
              )}
              {phase === "invalid" && (
                <StatusIndicator type="error">Validation failed</StatusIndicator>
              )}
              {phase === "committing" && (
                <StatusIndicator type="loading">Committing…</StatusIndicator>
              )}
              {phase === "done" && (
                <StatusIndicator type="success">Committed</StatusIndicator>
              )}
            </div>

            {phase === "invalid" && hasValidationErrors && (
              <SpaceBetween size="xs">
                <Box variant="h4">Validation Errors</Box>
                {candidate.validation!.map((v, i) => (
                  <Alert key={i} type={alertType(v.level)}>
                    {v.path && <><Box variant="code" display="inline-block">{v.path}</Box>{" "}</>}
                    {v.message}
                  </Alert>
                ))}
              </SpaceBetween>
            )}

            {hasDiff && (
              <>
                <Box variant="h4">Changes</Box>
                <UnifiedDiffView oldDoc={activeConfig!.document} newDoc={candidate.document!} />
              </>
            )}
          </SpaceBetween>
        )}
      </SpaceBetween>
    </Modal>
  );
}
