// Claude CLI provider — built-in AI provider
//
// Spawns `claude` CLI processes, parses stream-json output,
// translates to unified ProviderEvent stream.
//
// Supports: --resume, --mcp-config, --effort, --permission-prompt-tool
// Models: opus, sonnet, haiku, default + Bedrock ARNs
