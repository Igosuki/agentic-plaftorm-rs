import { useEffect, useState } from 'react'
import { ApiError, listAgents, login, runAgent } from './api'
import { fixtureAgents, fixtureFromLocation } from './fixtures'
import { LoginPanel } from './LoginPanel'
import { SurfaceView } from './SurfaceView'
import { TracePanel } from './TracePanel'
import type { AgentInfo, RunResponse, Session } from './types'

const SESSION_KEY = 'platform-ui.session'
const fixture = fixtureFromLocation()

function loadSession(): Session | null {
  try {
    const raw = sessionStorage.getItem(SESSION_KEY)
    return raw ? (JSON.parse(raw) as Session) : null
  } catch {
    return null
  }
}

function saveSession(s: Session | null) {
  try {
    if (s) sessionStorage.setItem(SESSION_KEY, JSON.stringify(s))
    else sessionStorage.removeItem(SESSION_KEY)
  } catch {
    // storage unavailable; session lives in memory only
  }
}

const errorTitle: Record<number, string> = {
  401: 'Unauthorized',
  422: 'Request not classified',
  429: 'Rate limited',
  502: 'Agent failed',
}

function ErrorBanner({ error }: { error: ApiError }) {
  const title = errorTitle[error.status] ?? (error.status ? `HTTP ${error.status}` : 'Network error')
  return (
    <div className="banner banner-bad" role="alert">
      <strong>
        {title} <code>{error.code}</code>
      </strong>
      <p>{error.message}</p>
      {error.status === 429 && <p>Wait a moment before retrying.</p>}
      {error.status === 401 && <p>Sign in again; the token is missing, invalid or expired.</p>}
    </div>
  )
}

function DisplayBanner({ response }: { response: RunResponse }) {
  const display = response.trace.display
  const status = display?.status ?? 'missing'
  return (
    <div className={`banner ${status === 'unavailable' ? 'banner-warn' : 'banner-bad'}`} role="alert">
      <strong>
        No UI surface: display {status}
      </strong>
      <p>{display?.reason ?? 'The gateway returned no UI and no reason. Showing the validated output instead.'}</p>
    </div>
  )
}

export default function App() {
  const [session, setSession] = useState<Session | null>(() => (fixture ? null : loadSession()))
  const [agents, setAgents] = useState<AgentInfo[]>(fixture ? fixtureAgents : [])
  const [agent, setAgent] = useState('')
  const [input, setInput] = useState(fixture ? 'What is the default retention period for the orders topic?' : '')
  const [busy, setBusy] = useState(false)
  const [response, setResponse] = useState<RunResponse | undefined>(fixture)
  const [error, setError] = useState<ApiError | undefined>()

  const signOut = () => {
    saveSession(null)
    setSession(null)
    setAgents([])
    setResponse(undefined)
    setError(undefined)
  }

  const fail = (e: unknown) => {
    const err = e instanceof ApiError ? e : new ApiError(0, 'client', (e as Error).message)
    if (err.status === 401 && session) signOut()
    setError(err)
  }

  useEffect(() => {
    if (!session) return
    listAgents(session.token).then(setAgents, fail)
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session])

  const onLogin = async (apiKey: string) => {
    setBusy(true)
    setError(undefined)
    try {
      const s = await login(apiKey)
      saveSession(s)
      setSession(s)
    } catch (e) {
      fail(e)
    } finally {
      setBusy(false)
    }
  }

  const onRun = async () => {
    if (fixture) return
    if (!session || !input.trim()) return
    setBusy(true)
    setError(undefined)
    setResponse(undefined)
    try {
      setResponse(await runAgent(session.token, input.trim(), agent || undefined))
    } catch (e) {
      fail(e)
    } finally {
      setBusy(false)
    }
  }

  const authed = fixture || session
  const traceSource = response ?? (error?.trace ? { trace: error.trace } : undefined)

  return (
    <div className="app">
      <header className="topbar">
        <h1>Agent Platform</h1>
        {fixture && <span className="pill pill-warn">fixture mode</span>}
        {session && (
          <div className="who">
            <span>
              {session.user}
              {session.team && <span className="muted"> / {session.team}</span>}
            </span>
            {session.roles.map((r) => (
              <span key={r} className="pill pill-info">
                {r}
              </span>
            ))}
            <button className="ghost" onClick={signOut}>
              Sign out
            </button>
          </div>
        )}
      </header>

      {!authed ? (
        <main className="narrow">
          <LoginPanel busy={busy} onLogin={onLogin} />
          {error && <ErrorBanner error={error} />}
        </main>
      ) : (
        <main>
          <form
            className="panel prompt"
            onSubmit={(e) => {
              e.preventDefault()
              void onRun()
            }}
          >
            <label htmlFor="prompt">Prompt</label>
            <textarea
              id="prompt"
              rows={3}
              value={input}
              onChange={(e) => setInput(e.target.value)}
              placeholder="Ask the platform something"
            />
            <div className="prompt-actions">
              <label htmlFor="agent" className="inline">
                Agent
              </label>
              <select id="agent" value={agent} onChange={(e) => setAgent(e.target.value)}>
                <option value="">auto-route</option>
                {agents.map((a) => (
                  <option key={a.name} value={a.name} title={a.description}>
                    {a.name}
                  </option>
                ))}
              </select>
              <button type="submit" disabled={busy || !input.trim() || !!fixture}>
                {busy ? 'Running...' : fixture ? 'Run (disabled in fixture mode)' : 'Run'}
              </button>
            </div>
          </form>

          {error && <ErrorBanner error={error} />}

          {(response || error?.trace) && (
            <div className="result">
              <section className="surface" aria-label="Result">
                {response?.ui ? (
                  <div className="panel">
                    <SurfaceView ui={response.ui} />
                  </div>
                ) : (
                  response && (
                    <>
                      <DisplayBanner response={response} />
                      <div className="panel">
                        <h2>Validated output</h2>
                        <pre className="json">{JSON.stringify(response.output, null, 2)}</pre>
                      </div>
                    </>
                  )
                )}
              </section>
              <TracePanel
                trace={traceSource?.trace}
                requestId={response?.request_id}
                agent={response?.agent}
                promptRef={response?.prompt}
                error={error}
              />
            </div>
          )}
        </main>
      )}
    </div>
  )
}
