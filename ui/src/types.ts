import type { A2uiMessage } from '@zavora-ai/adk-ui-react'

export interface Usage {
  input_tokens: number
  output_tokens: number
  cost_usd: number
}

export interface Attempt {
  attempt: number
  outcome: string
  detail?: string
}

export interface ToolCall {
  server: string
  tool: string
  outcome: 'ok' | 'error' | 'denied' | string
  latency_ms: number
}

export interface PromptRef {
  id: string
  version: string
}

export interface DisplayTrace {
  status: 'rendered' | 'failed' | 'unavailable' | string
  attempts?: number
  reason?: string | null
  prompt?: PromptRef
  usage?: Usage
}

export interface Trace {
  engine?: string
  classification?: { method: string; scores: Record<string, number> }
  attempts?: Attempt[]
  tool_calls?: ToolCall[]
  usage?: Usage
  latency_ms?: number
  display?: DisplayTrace
}

export interface A2uiPayload {
  protocol: 'a2ui'
  version: string
  surface_id: string
  messages: A2uiMessage[]
}

export interface RunResponse {
  agent: string
  output: unknown
  prompt: PromptRef
  request_id: string
  trace: Trace
  ui: A2uiPayload | null
}

export interface AgentInfo {
  name: string
  description: string
  prompt_id: string
  mcp_servers: string[]
}

export interface Session {
  token: string
  user: string
  team: string
  roles: string[]
}
