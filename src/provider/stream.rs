// Unified provider event stream
//
// Every provider emits ProviderEvent values. Peckboard maps
// them to event log entries. The provider is responsible for
// translating its native format into this unified stream.
//
// ProviderEvent kinds:
//   Started     — agent initialized (model, conversation_id)
//   Text        — streamed text chunk
//   ToolStart   — agent invoked a tool (id, name, input)
//   ToolEnd     — tool finished (id, output, error)
//   Completed   — agent finished normally
//   Crashed     — agent failed (reason, exit_code, stderr)
//   ControlRequest — permission prompt or user question
