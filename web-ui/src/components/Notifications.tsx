import { createContext, useCallback, useContext, useState } from "react";
import Flashbar, { FlashbarProps } from "@cloudscape-design/components/flashbar";

type FlashType = "success" | "error" | "warning" | "info";

interface NotificationsContextValue {
  notify: (type: FlashType, content: string) => void;
}

const NotificationsContext = createContext<NotificationsContextValue>({
  notify: () => {},
});

export function useNotifications() {
  return useContext(NotificationsContext);
}

let nextId = 0;

export function NotificationsProvider({ children }: { children: React.ReactNode }) {
  const [items, setItems] = useState<FlashbarProps.MessageDefinition[]>([]);

  const dismiss = useCallback((id: string) => {
    setItems((prev) => prev.filter((item) => item.id !== id));
  }, []);

  const notify = useCallback((type: FlashType, content: string) => {
    const id = String(++nextId);
    const item: FlashbarProps.MessageDefinition = {
      id,
      type,
      content,
      dismissible: true,
      onDismiss: () => dismiss(id),
    };
    setItems((prev) => [...prev, item]);
    setTimeout(() => dismiss(id), 5000);
  }, [dismiss]);

  return (
    <NotificationsContext.Provider value={{ notify }}>
      {children}
      <div style={{ position: "fixed", top: 48, right: 16, zIndex: 9999, width: 420, maxWidth: "calc(100vw - 32px)" }}>
        <Flashbar items={items} stackItems />
      </div>
    </NotificationsContext.Provider>
  );
}
