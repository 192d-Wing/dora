import { useState } from "react";
import Box from "@cloudscape-design/components/box";
import Button from "@cloudscape-design/components/button";
import Container from "@cloudscape-design/components/container";
import FormField from "@cloudscape-design/components/form-field";
import Header from "@cloudscape-design/components/header";
import Input from "@cloudscape-design/components/input";
import Select from "@cloudscape-design/components/select";
import SpaceBetween from "@cloudscape-design/components/space-between";
import ContentLayout from "@cloudscape-design/components/content-layout";
import Alert from "@cloudscape-design/components/alert";
import Toggle from "@cloudscape-design/components/toggle";
import ColumnLayout from "@cloudscape-design/components/column-layout";
import Modal from "@cloudscape-design/components/modal";
import { post } from "../api";

interface ActionResponse {
  status?: string;
  action?: string;
  message?: string;
  operation_id?: string;
}

function useAction() {
  const [loading, setLoading] = useState(false);
  const [result, setResult] = useState<ActionResponse | null>(null);
  const [error, setError] = useState<string | null>(null);

  const run = async (path: string, body?: Record<string, unknown>) => {
    setLoading(true);
    setError(null);
    setResult(null);
    try {
      const res = await post<ActionResponse>(path, body);
      setResult(res);
    } catch (err) {
      setError(String(err));
    } finally {
      setLoading(false);
    }
  };

  const clear = () => {
    setResult(null);
    setError(null);
  };

  return { loading, result, error, run, clear };
}

function ResultBanner({ result, error, onDismiss }: {
  result: ActionResponse | null;
  error: string | null;
  onDismiss: () => void;
}) {
  if (error) {
    return <Alert type="error" dismissible onDismiss={onDismiss}>{error}</Alert>;
  }
  if (result) {
    return (
      <Alert
        type={result.status === "succeeded" || result.status === "accepted" ? "success" : "error"}
        dismissible
        onDismiss={onDismiss}
      >
        {result.message ?? result.status ?? "Action completed"}
        {result.operation_id && ` (operation: ${result.operation_id})`}
      </Alert>
    );
  }
  return null;
}

function MaintenanceMode() {
  const [enabled, setEnabled] = useState(false);
  const [reason, setReason] = useState("");
  const { loading, result, error, run, clear } = useAction();

  return (
    <Container header={<Header variant="h2">Maintenance Mode</Header>}>
      <SpaceBetween size="m">
        <ResultBanner result={result} error={error} onDismiss={clear} />
        <ColumnLayout columns={2}>
          <FormField label="Enable maintenance mode">
            <Toggle
              checked={enabled}
              onChange={({ detail }) => setEnabled(detail.checked)}
            >
              {enabled ? "Enabled" : "Disabled"}
            </Toggle>
          </FormField>
          <FormField label="Reason (optional)">
            <Input
              value={reason}
              onChange={({ detail }) => setReason(detail.value)}
              placeholder="Scheduled maintenance window"
            />
          </FormField>
        </ColumnLayout>
        <Button
          variant="primary"
          loading={loading}
          onClick={() =>
            run("/v1/actions/maintenance-mode", {
              enabled,
              ...(reason ? { reason } : {}),
            })
          }
        >
          Apply
        </Button>
      </SpaceBetween>
    </Container>
  );
}

function ReloadConfig() {
  const { loading, result, error, run, clear } = useAction();

  return (
    <Container header={<Header variant="h2">Reload Configuration</Header>}>
      <SpaceBetween size="m">
        <ResultBanner result={result} error={error} onDismiss={clear} />
        <Box>Reload the active configuration from disk.</Box>
        <Button variant="primary" loading={loading} onClick={() => run("/v1/actions/reload", {})}>
          Reload
        </Button>
      </SpaceBetween>
    </Container>
  );
}

function DrainServer() {
  const [reason, setReason] = useState("");
  const { loading, result, error, run, clear } = useAction();

  return (
    <Container header={<Header variant="h2">Drain</Header>}>
      <SpaceBetween size="m">
        <ResultBanner result={result} error={error} onDismiss={clear} />
        <Box>Enter drain mode. New leases are suppressed; existing clients may still renew.</Box>
        <FormField label="Reason (optional)">
          <Input
            value={reason}
            onChange={({ detail }) => setReason(detail.value)}
            placeholder="Pre-upgrade drain"
          />
        </FormField>
        <Button
          variant="primary"
          loading={loading}
          onClick={() => run("/v1/actions/drain", reason ? { reason } : {})}
        >
          Drain
        </Button>
      </SpaceBetween>
    </Container>
  );
}

function ReleaseLease() {
  const [family, setFamily] = useState({ label: "v4", value: "v4" });
  const [ip, setIp] = useState("");
  const [ddnsCleanup, setDdnsCleanup] = useState(false);
  const { loading, result, error, run, clear } = useAction();

  return (
    <Container header={<Header variant="h2">Release Lease</Header>}>
      <SpaceBetween size="m">
        <ResultBanner result={result} error={error} onDismiss={clear} />
        <ColumnLayout columns={3}>
          <FormField label="Family">
            <Select
              selectedOption={family}
              onChange={({ detail }) =>
                setFamily(detail.selectedOption as typeof family)
              }
              options={[
                { label: "v4", value: "v4" },
                { label: "v6", value: "v6" },
              ]}
            />
          </FormField>
          <FormField label="IP Address">
            <Input
              value={ip}
              onChange={({ detail }) => setIp(detail.value)}
              placeholder="192.168.1.100"
            />
          </FormField>
          <FormField label="DDNS Cleanup">
            <Toggle
              checked={ddnsCleanup}
              onChange={({ detail }) => setDdnsCleanup(detail.checked)}
            >
              {ddnsCleanup ? "Yes" : "No"}
            </Toggle>
          </FormField>
        </ColumnLayout>
        <Button
          variant="primary"
          loading={loading}
          disabled={!ip}
          onClick={() =>
            run("/v1/actions/release-lease", {
              family: family.value,
              ip,
              ddns_cleanup: ddnsCleanup,
            })
          }
        >
          Release
        </Button>
      </SpaceBetween>
    </Container>
  );
}

function ShutdownServer() {
  const [visible, setVisible] = useState(false);
  const [gracePeriod, setGracePeriod] = useState("30");
  const [reason, setReason] = useState("");
  const { loading, result, error, run, clear } = useAction();

  return (
    <Container header={<Header variant="h2">Shutdown</Header>}>
      <SpaceBetween size="m">
        <ResultBanner result={result} error={error} onDismiss={clear} />
        <Box>Initiate a graceful shutdown of the DHCP server. This action is irreversible.</Box>
        <ColumnLayout columns={2}>
          <FormField label="Grace period (seconds)">
            <Input
              type="number"
              value={gracePeriod}
              onChange={({ detail }) => setGracePeriod(detail.value)}
            />
          </FormField>
          <FormField label="Reason (optional)">
            <Input
              value={reason}
              onChange={({ detail }) => setReason(detail.value)}
              placeholder="Planned restart"
            />
          </FormField>
        </ColumnLayout>
        <Button variant="primary" onClick={() => setVisible(true)}>
          Shutdown...
        </Button>
        <Modal
          visible={visible}
          onDismiss={() => setVisible(false)}
          header="Confirm shutdown"
          footer={
            <Box float="right">
              <SpaceBetween direction="horizontal" size="xs">
                <Button variant="link" onClick={() => setVisible(false)}>Cancel</Button>
                <Button
                  variant="primary"
                  loading={loading}
                  onClick={() => {
                    run("/v1/actions/shutdown", {
                      grace_period_seconds: parseInt(gracePeriod, 10) || 30,
                      ...(reason ? { reason } : {}),
                    }).then(() => setVisible(false));
                  }}
                >
                  Confirm shutdown
                </Button>
              </SpaceBetween>
            </Box>
          }
        >
          Are you sure you want to shut down the server? This will stop all DHCP
          services after the grace period ({gracePeriod}s).
        </Modal>
      </SpaceBetween>
    </Container>
  );
}

export default function Actions() {
  return (
    <ContentLayout header={<Header variant="h1">Actions</Header>}>
      <SpaceBetween size="l">
        <ReloadConfig />
        <MaintenanceMode />
        <DrainServer />
        <ReleaseLease />
        <ShutdownServer />
      </SpaceBetween>
    </ContentLayout>
  );
}
