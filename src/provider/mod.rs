pub mod claude;
pub mod registry;
pub mod stream;

// Provider factory — AI provider abstraction
//
// Providers implement the full agent lifecycle: spawn, send,
// interrupt, kill, cleanup. Each provider translates its native
// output into the unified ProviderEvent stream. Claude CLI is
// the built-in provider; plugins can register additional providers.
