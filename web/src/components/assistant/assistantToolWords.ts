import type { Event } from '../../types/api'

type Input = Record<string, unknown> | undefined

/** A tool the Assistant has running right now. */
export interface ToolInFlight {
  key: string
  name: string
  input: Input
}

/** The bare tool name: `mcp__peckboard__send_message` → `send_message`. */
export function bareToolName(name: string): string {
  return name.includes('__') ? (name.split('__').pop() ?? name) : name
}

/** The newest tool call of the Assistant's current turn that has not
 *  ended yet; `null` between turns or when no tool is running. Built-in
 *  subagents' own calls are not the Assistant's. */
export function toolInFlight(events: Event[]): ToolInFlight | null {
  const open = new Map<string, ToolInFlight>()
  for (const ev of events) {
    if (ev.kind === 'agent-start' || ev.kind === 'agent-end') {
      open.clear()
      continue
    }
    if (ev.data.parent_tool_use_id || ev.data.parentToolUseId) continue
    const id = (ev.data.toolUseId as string) ?? (ev.data.tool_use_id as string) ?? ev.id
    if (ev.kind === 'agent-tool-start') {
      const name = (ev.data.name as string) ?? (ev.data.tool_name as string) ?? 'tool'
      // The CLI emits a streaming and a snapshot start per call; the later
      // one carries the full input.
      open.delete(id)
      open.set(id, { key: id, name: bareToolName(name), input: ev.data.input as Input })
    } else if (ev.kind === 'agent-tool-end') {
      open.delete(id)
    }
  }
  const all = [...open.values()]
  return all.length > 0 ? all[all.length - 1] : null
}

function str(input: Input, ...keys: string[]): string {
  for (const k of keys) {
    const v = input?.[k]
    if (typeof v === 'string' && v.trim()) return v.trim()
  }
  return ''
}

/** "Messaging stashify…": the tool call in plain words. `sessionName`
 *  resolves a session id from the input to its name. */
export function describeTool(
  tool: ToolInFlight,
  sessionName: (id: string) => string | null,
): string {
  const target = () => {
    const id = str(tool.input, 'session_id', 'target_session_id')
    return str(tool.input, 'session_name', 'session') || (id && sessionName(id)) || 'a session'
  }
  switch (tool.name) {
    case 'send_message':
      return `Messaging ${target()}…`
    case 'read_session':
      return `Reading ${target()}…`
    case 'interrupt_session':
      return `Interrupting ${target()}…`
    case 'clear_session':
      return `Clearing ${target()}…`
    case 'terminate_agent':
      return `Stopping ${target()}…`
    case 'create_session':
      return 'Starting a new session…'
    case 'find_session':
    case 'list_sessions':
    case 'search_sessions':
      return 'Looking for the right session…'
    case 'answer_question':
      return 'Answering a question…'
    case 'search_web':
      return 'Searching the web…'
    case 'fetch_web':
    case 'fetch_url':
    case 'parse_web':
    case 'web_get_part':
      return 'Reading a web page…'
    case 'create_card':
      return 'Creating a task…'
    case 'update_card':
    case 'move_card_to_done':
    case 'move_card_to_wont_do':
      return 'Updating a task…'
    case 'list_cards':
      return 'Checking the board…'
    case 'create_repeating_task':
      return 'Scheduling that…'
    case 'update_repeating_task':
      return 'Changing the schedule…'
    case 'delete_repeating_task':
      return 'Cancelling a scheduled task…'
    case 'list_repeating_tasks':
      return 'Checking your schedule…'
    case 'show_view':
    case 'ui_open':
      return 'Opening it on screen…'
    case 'voice_queue':
      return 'Checking what is waiting…'
    case 'run_command':
    case 'run_tests':
    case 'run_background':
      return 'Running a command…'
    case 'ssh_run':
    case 'ssh_run_many':
      return 'Running a command on a server…'
    case 'read_file':
    case 'list_files':
    case 'search_files':
    case 'file_outline':
    case 'read_symbol':
      return 'Reading files…'
    case 'memory_add':
    case 'memory_update':
    case 'memory_remove':
    case 'memory_list':
      return 'Checking my notes…'
    default:
      return `Using ${tool.name.replace(/_/g, ' ')}…`
  }
}
