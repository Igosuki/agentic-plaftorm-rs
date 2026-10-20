import { useState } from 'react'

const demoKeys = [
  { key: 'alice-key', label: 'alice (reader)' },
  { key: 'bob-key', label: 'bob (engineer)' },
  { key: 'carol-key', label: 'carol (admin)' },
  { key: 'dave-key', label: 'dave (reader)' },
]

interface Props {
  busy: boolean
  onLogin: (apiKey: string) => void
}

export function LoginPanel({ busy, onLogin }: Props) {
  const [key, setKey] = useState('')
  return (
    <form
      className="panel login"
      onSubmit={(e) => {
        e.preventDefault()
        if (key.trim()) onLogin(key.trim())
      }}
    >
      <h2>Sign in</h2>
      <label htmlFor="api-key">API key</label>
      <input
        id="api-key"
        type="password"
        autoComplete="off"
        value={key}
        onChange={(e) => setKey(e.target.value)}
        placeholder="alice-key"
      />
      {import.meta.env.DEV && (
        <div className="presets" role="group" aria-label="Demo keys">
          {demoKeys.map((d) => (
            <button type="button" key={d.key} className="ghost" onClick={() => setKey(d.key)}>
              {d.label}
            </button>
          ))}
        </div>
      )}
      <button type="submit" disabled={busy || !key.trim()}>
        {busy ? 'Signing in...' : 'Sign in'}
      </button>
    </form>
  )
}
