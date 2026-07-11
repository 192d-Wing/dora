import { useState } from "react";

export interface TablePrefs {
  pageSize: number;
  wrapLines: boolean;
  visibleColumns: string[];
}

const STORAGE_PREFIX = "dora_table_prefs_";

function loadPrefs(key: string, defaults: TablePrefs): TablePrefs {
  try {
    const raw = localStorage.getItem(STORAGE_PREFIX + key);
    if (raw) return { ...defaults, ...JSON.parse(raw) };
  } catch { /* use defaults */ }
  return defaults;
}

export function useTablePreferences(key: string, defaults: TablePrefs) {
  const [prefs, setPrefs] = useState<TablePrefs>(() => loadPrefs(key, defaults));

  const update = (next: TablePrefs) => {
    setPrefs(next);
    localStorage.setItem(STORAGE_PREFIX + key, JSON.stringify(next));
  };

  return [prefs, update] as const;
}
