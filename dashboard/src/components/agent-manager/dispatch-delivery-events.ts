import { DELIVERY_EVENT_STATUS_TONES } from "../../theme/statusTokens";

export const DELIVERY_EVENT_STATUS_STYLE = DELIVERY_EVENT_STATUS_TONES;

export function getDeliveryEventStatusStyle(status: string): { bg: string; text: string } {
  return DELIVERY_EVENT_STATUS_TONES[status as keyof typeof DELIVERY_EVENT_STATUS_TONES]
    ?? { bg: "rgba(148,163,184,0.10)", text: "#94a3b8" };
}

export function deliveryEventMessagesCount(value: unknown): number {
  return Array.isArray(value) ? value.length : 0;
}

export function compactStringParts(parts: Array<string | null | undefined | false>): string[] {
  return parts.filter((part): part is string => Boolean(part));
}

export function summarizeDeliveryError(error: string | null | undefined): string {
  if (!error?.trim()) return "-";
  const compact = error.trim().replace(/\s+/g, " ");
  return compact.length > 96 ? `${compact.slice(0, 93)}...` : compact;
}

export interface DeliveryEventsLoadState<TEvent> {
  events: TEvent[];
  loading: boolean;
  error: string | null;
  loadedDispatchId: string | null;
}

export function createDeliveryEventsLoadState<TEvent>(): DeliveryEventsLoadState<TEvent> {
  return {
    events: [],
    loading: false,
    error: null,
    loadedDispatchId: null,
  };
}

export function startDeliveryEventsLoad<TEvent>(
  state: DeliveryEventsLoadState<TEvent>,
  dispatchId: string,
  reset: boolean,
): DeliveryEventsLoadState<TEvent> {
  const hasCurrentRows = state.loadedDispatchId === dispatchId && state.events.length > 0;
  return {
    events: reset ? [] : state.events,
    loading: reset || !hasCurrentRows,
    error: null,
    loadedDispatchId: reset ? null : state.loadedDispatchId,
  };
}

export function finishDeliveryEventsLoadSuccess<TEvent>(
  state: DeliveryEventsLoadState<TEvent>,
  dispatchId: string,
  events: TEvent[],
): DeliveryEventsLoadState<TEvent> {
  return {
    ...state,
    events,
    loading: false,
    error: null,
    loadedDispatchId: dispatchId,
  };
}

export function finishDeliveryEventsLoadError<TEvent>(
  state: DeliveryEventsLoadState<TEvent>,
  message: string,
  clearEvents: boolean,
): DeliveryEventsLoadState<TEvent> {
  return {
    ...state,
    events: clearEvents ? [] : state.events,
    loading: false,
    error: message,
    loadedDispatchId: clearEvents ? null : state.loadedDispatchId,
  };
}
