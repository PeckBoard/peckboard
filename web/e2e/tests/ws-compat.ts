import { WebSocket as NodeWebSocket } from 'ws'

/**
 * Browser-style WebSocket constructor that also works on Node < 22, where
 * no global `WebSocket` exists: fall back to the `ws` package, which
 * implements the same event API (addEventListener / send / close) the
 * specs use.
 */
export const WebSocketImpl = (globalThis.WebSocket ?? NodeWebSocket) as typeof NodeWebSocket

/** Minimal message-event shape common to the global and `ws` implementations. */
export type WsMessageEvent = { data: unknown }
