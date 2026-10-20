import type { AgentInfo, RunResponse, Session, Trace } from './types'

export class ApiError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message: string,
    readonly requestId?: string,
    readonly trace?: Trace,
  ) {
    super(message)
  }
}

async function request<T>(path: string, init: RequestInit): Promise<T> {
  let res: Response
  try {
    res = await fetch(path, init)
  } catch (e) {
    throw new ApiError(0, 'network', `gateway unreachable: ${(e as Error).message}`)
  }
  const text = await res.text()
  let body: unknown
  try {
    body = text ? JSON.parse(text) : undefined
  } catch {
    body = undefined
  }
  if (!res.ok) {
    const b = (body ?? {}) as {
      error?: { code?: string; message?: string }
      request_id?: string
      trace?: Trace
    }
    throw new ApiError(
      res.status,
      b.error?.code ?? 'http_error',
      b.error?.message ?? (text.slice(0, 200) || res.statusText),
      b.request_id,
      b.trace,
    )
  }
  return body as T
}

function decodeClaims(token: string): { sub?: string; team?: string; roles?: string[] } {
  try {
    const payload = token.split('.')[1].replace(/-/g, '+').replace(/_/g, '/')
    return JSON.parse(atob(payload))
  } catch {
    return {}
  }
}

export async function login(apiKey: string): Promise<Session> {
  const { access_token } = await request<{ access_token: string }>('/auth/token', {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ api_key: apiKey }),
  })
  const claims = decodeClaims(access_token)
  return {
    token: access_token,
    user: claims.sub ?? 'unknown',
    team: claims.team ?? '',
    roles: claims.roles ?? [],
  }
}

const bearer = (token: string) => ({ authorization: `Bearer ${token}` })

export const listAgents = (token: string) =>
  request<AgentInfo[]>('/v1/agents', { headers: bearer(token) })

export const runAgent = (token: string, input: string, agentHint?: string) =>
  request<RunResponse>('/v1/agents/run', {
    method: 'POST',
    headers: { ...bearer(token), 'content-type': 'application/json' },
    body: JSON.stringify({
      input,
      ui: 'a2ui',
      ...(agentHint ? { agent_hint: agentHint } : {}),
    }),
  })
