import { useCallback, useEffect, useRef, useState } from "react";
import Button from "@cloudscape-design/components/button";
import ButtonDropdown from "@cloudscape-design/components/button-dropdown";
import SpaceBetween from "@cloudscape-design/components/space-between";
import Box from "@cloudscape-design/components/box";

const INTERVAL_OPTIONS = [
  { id: "0", text: "Off" },
  { id: "10", text: "10 s" },
  { id: "15", text: "15 s" },
  { id: "30", text: "30 s" },
  { id: "60", text: "1 min" },
  { id: "300", text: "5 min" },
];

export function useAutoRefresh(load: () => void, defaultSeconds = 30) {
  const [intervalSeconds, setIntervalSeconds] = useState(defaultSeconds);
  const [paused, setPaused] = useState(false);
  const timerRef = useRef<ReturnType<typeof setInterval> | null>(null);

  const clear = useCallback(() => {
    if (timerRef.current) {
      clearInterval(timerRef.current);
      timerRef.current = null;
    }
  }, []);

  useEffect(() => {
    clear();
    if (intervalSeconds > 0 && !paused) {
      timerRef.current = setInterval(load, intervalSeconds * 1000);
    }
    return clear;
  }, [intervalSeconds, paused, load, clear]);

  return { intervalSeconds, setIntervalSeconds, paused, setPaused };
}

interface RefreshControlProps {
  onRefresh: () => void;
  intervalSeconds: number;
  onIntervalChange: (seconds: number) => void;
  paused: boolean;
  onPausedChange: (paused: boolean) => void;
}

export default function RefreshControl({
  onRefresh,
  intervalSeconds,
  onIntervalChange,
  paused,
  onPausedChange,
}: Readonly<RefreshControlProps>) {
  const label = INTERVAL_OPTIONS.find((o) => o.id === String(intervalSeconds))?.text ?? `${intervalSeconds}s`;

  return (
    <SpaceBetween direction="horizontal" size="xs">
      {intervalSeconds > 0 && (
        <Button
          iconName={paused ? "play" : "pause"}
          variant="icon"
          onClick={() => onPausedChange(!paused)}
          ariaLabel={paused ? "Resume auto-refresh" : "Pause auto-refresh"}
        />
      )}
      <ButtonDropdown
        items={INTERVAL_OPTIONS}
        onItemClick={({ detail }) => {
          onIntervalChange(Number(detail.id));
          onPausedChange(false);
        }}
      >
        <Box fontSize="body-s" color={paused ? "text-status-inactive" : "text-status-info"}>
          {intervalSeconds === 0 ? "Auto: off" : paused ? `Auto: ${label} (paused)` : `Auto: ${label}`}
        </Box>
      </ButtonDropdown>
      <Button iconName="refresh" onClick={onRefresh} ariaLabel="Refresh now" />
    </SpaceBetween>
  );
}
