/** The canned prompt behind the "Plugin evals → Init eval suite" session
 *  menu action (claude provider only). `claude plugin eval init` is an
 *  interactive terminal interview, so the server can't run it headless;
 *  the plugin-evals docs' sanctioned alternative is asking Claude to do
 *  the same work from an open session — this prompt is that ask. See
 *  https://code.claude.com/docs/en/plugin-evals.
 */
export const PLUGIN_EVAL_INIT_PROMPT = `Author an eval suite for this project's Claude Code plugin(s) — the equivalent of running \`claude plugin eval init\`, done inside this session.

1. Discover plugins. Look for \`.claude-plugin/plugin.json\` at the repo root and in plugin directories (a marketplace checkout may hold several). If none exist, say so and stop — do not invent one.
2. Before writing anything, post a breakdown of each plugin: its commands, agents, skills, hooks, and MCP servers, with one line on what each does. If there are several plugins, ask me which to target.
3. Interview me (one question at a time) about the behaviours worth locking in: the plugin's main jobs, realistic user prompts, what a good result looks like, and any failure modes I care about. Propose the eval cases and graders, and let me adjust before writing files.
4. Write the suite under the plugin's \`evals/\` directory (or the directory named by the manifest's \`experimental.evals\` key, if set), in the documented layout: each case is a directory holding \`prompt.md\` (frontmatter + the user prompt) and/or a \`case.yaml\` (requires \`schema_version: "1.1"\` and \`name\`; use its \`context.*\` fields for fixtures and workspace setup), plus \`graders/*.md\` for judge-scored criteria. Group related cases under a parent directory that is not itself a case.
5. Finish with a breakdown of what you wrote — cases, graders, tags — and the command to run it: \`claude plugin eval <path-or-name>\` (add \`--json\` for machine-readable results).

Consult https://code.claude.com/docs/en/plugin-evals for field details. Do not run \`claude plugin eval init\` itself — it requires an interactive terminal; author the files directly.`
