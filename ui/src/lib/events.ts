// SSE client for /api/ui/events with reconnect and exponential backoff.
import type { UiEvent } from './api/types';

export type Listener = (e: UiEvent, receivedAt: number) => void;

const KINDS = ['recall', 'store', 'link', 'reinforce', 'forget', 'phase_change', 'sweep', 'lagged'] as const;

export function connectEvents(onEvent: Listener, onState?: (connected: boolean) => void): () => void {
  let source: EventSource | null = null;
  let delay = 500;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let closed = false;
  const open = () => {
    if (closed) return;
    source = new EventSource('/api/ui/events', { withCredentials: true });
    source.onopen = () => {
      delay = 500;
      onState?.(true);
    };
    for (const kind of KINDS) {
      source.addEventListener(kind, (m) => {
        let event: UiEvent;
        try {
          event = JSON.parse((m as MessageEvent).data) as UiEvent;
        } catch {
          return; // ignore malformed frames; handler errors still surface
        }
        onEvent(event, performance.now());
      });
    }
    source.onerror = () => {
      onState?.(false);
      source?.close();
      timer = setTimeout(open, delay);
      delay = Math.min(delay * 2, 15_000);
    };
  };
  open();
  return () => {
    closed = true;
    clearTimeout(timer);
    source?.close();
  };
}
