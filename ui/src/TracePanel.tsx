import type { ApiError } from './api'
import type { Attempt, DisplayTrace, ToolCall, Trace, Usage } from './types'

const money = (n: number) => `$${n.toFixed(5)}`

function UsageLine({ usage }: { usage?: Usage }) {
  if (!usage) return <span className="muted">no usage reported</span>
  return (
    <span>
      {usage.input_tokens.toLocaleString()} in / {usage.output_tokens.toLocaleString()} out
      <span className="cost">{money(usage.cost_usd)}</span>
    </span>
  )
}

function Pill({ kind, children }: { kind: string; children: React.ReactNode }) {
  return <span className={`pill pill-${kind}`}>{children}</span>
}

const attemptKind = (o: string) => (o === 'ok' ? 'ok' : 'bad')
const toolKind = (o: ToolCall['outcome']) => (o === 'ok' ? 'ok' : o === 'denied' ? 'warn' : 'bad')
const displayKind = (s: string) => (s === 'rendered' ? 'ok' : s === 'unavailable' ? 'warn' : 'bad')

function Attempts({ attempts }: { attempts: Attempt[] }) {
  if (!attempts.length) return <p className="muted">none</p>
  return (
    <ol className="rows">
      {attempts.map((a) => (
        <li key={a.attempt}>
          <div className="row-head">
            <span>Attempt {a.attempt}</span>
            <Pill kind={attemptKind(a.outcome)}>{a.outcome}</Pill>
          </div>
          {a.detail && <p className="detail">{a.detail}</p>}
        </li>
      ))}
    </ol>
  )
}

function ToolCalls({ calls }: { calls: ToolCall[] }) {
  if (!calls.length) return <p className="muted">no tool calls</p>
  return (
    <ul className="rows">
      {calls.map((c, i) => (
        <li key={i}>
          <div className="row-head">
            <code>
              {c.server}.{c.tool}
            </code>
            <span>
              <Pill kind={toolKind(c.outcome)}>{c.outcome}</Pill>
              <span className="muted"> {c.latency_ms} ms</span>
            </span>
          </div>
        </li>
      ))}
    </ul>
  )
}

function Display({ display }: { display?: DisplayTrace }) {
  if (!display) return <p className="muted">no display stage reported</p>
  return (
    <dl className="kv">
      <dt>Status</dt>
      <dd>
        <Pill kind={displayKind(display.status)}>{display.status}</Pill>
      </dd>
      <dt>Prompt</dt>
      <dd>{display.prompt ? <code>{display.prompt.id}@{display.prompt.version}</code> : <span className="muted">n/a</span>}</dd>
      <dt>Attempts</dt>
      <dd>{display.attempts ?? 0}</dd>
      {display.reason && (
        <>
          <dt>Reason</dt>
          <dd className="detail">{display.reason}</dd>
        </>
      )}
      <dt>Usage</dt>
      <dd>
        <UsageLine usage={display.usage} />
      </dd>
    </dl>
  )
}

interface Props {
  trace?: Trace
  requestId?: string
  agent?: string
  promptRef?: { id: string; version: string }
  error?: ApiError
}

export function TracePanel({ trace, requestId, agent, promptRef, error }: Props) {
  const totalCost = (trace?.usage?.cost_usd ?? 0) + (trace?.display?.usage?.cost_usd ?? 0)
  const scores = trace?.classification?.scores

  return (
    <aside className="panel trace" aria-label="Request trace">
      <h2>Trace</h2>
      {error && (
        <p className="detail">
          Request failed; the trace below is what the gateway returned with the error.
        </p>
      )}
      <dl className="kv">
        <dt>Request</dt>
        <dd>
          <code className="wrap">{requestId ?? error?.requestId ?? 'n/a'}</code>
        </dd>
        {agent && (
          <>
            <dt>Agent</dt>
            <dd>
              <code>{agent}</code>
            </dd>
          </>
        )}
        {promptRef && (
          <>
            <dt>Prompt</dt>
            <dd>
              <code>{promptRef.id}@{promptRef.version}</code>
            </dd>
          </>
        )}
        <dt>Engine</dt>
        <dd>{trace?.engine ?? <span className="muted">n/a</span>}</dd>
        {trace?.latency_ms !== undefined && (
          <>
            <dt>Latency</dt>
            <dd>{trace.latency_ms.toLocaleString()} ms</dd>
          </>
        )}
        <dt>Total cost</dt>
        <dd className="strong">{money(totalCost)}</dd>
      </dl>

      {trace?.classification && (
        <section>
          <h3>Routing ({trace.classification.method})</h3>
          {scores && Object.keys(scores).length ? (
            <ul className="rows">
              {Object.entries(scores)
                .sort((a, b) => b[1] - a[1])
                .map(([name, score]) => (
                  <li key={name} className="row-head">
                    <code>{name}</code>
                    <span>{score}</span>
                  </li>
                ))}
            </ul>
          ) : (
            <p className="muted">no keyword matches</p>
          )}
        </section>
      )}

      {trace?.attempts && (
        <section>
          <h3>Specialist attempts</h3>
          <Attempts attempts={trace.attempts} />
        </section>
      )}

      {trace?.tool_calls && (
        <section>
          <h3>Tool calls</h3>
          <ToolCalls calls={trace.tool_calls} />
        </section>
      )}

      {trace?.usage && (
        <section>
          <h3>Specialist usage</h3>
          <UsageLine usage={trace.usage} />
        </section>
      )}

      {!error && (
        <section>
          <h3>Display agent</h3>
          <Display display={trace?.display} />
        </section>
      )}
    </aside>
  )
}
