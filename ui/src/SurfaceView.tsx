import { A2uiStore, A2uiSurfaceRenderer, applyParsedMessages } from '@zavora-ai/adk-ui-react'
import { Component, useMemo, type ReactNode } from 'react'
import type { A2uiPayload } from './types'
import { useDarkMode } from './useDarkMode'

function SurfaceError({ message }: { message: string }) {
  return (
    <div className="banner banner-bad" role="alert">
      <strong>UI surface could not be rendered</strong>
      <p>{message}</p>
    </div>
  )
}

class Boundary extends Component<{ children: ReactNode }, { error?: Error }> {
  state: { error?: Error } = {}

  static getDerivedStateFromError(error: Error) {
    return { error }
  }

  render() {
    return this.state.error ? <SurfaceError message={this.state.error.message} /> : this.props.children
  }
}

function Surface({ ui }: { ui: A2uiPayload }) {
  const dark = useDarkMode()
  const parsed = useMemo(() => {
    try {
      const store = new A2uiStore()
      applyParsedMessages(
        store,
        ui.messages.map((message) => ({ message, raw: JSON.stringify(message) })),
      )
      return { store }
    } catch (e) {
      return { error: (e as Error).message }
    }
  }, [ui])

  if (parsed.error !== undefined) return <SurfaceError message={parsed.error} />
  return (
    <A2uiSurfaceRenderer
      store={parsed.store}
      surfaceId={ui.surface_id}
      theme={dark ? 'dark' : 'light'}
      onAction={(event) => console.info('a2ui action', event)}
    />
  )
}

export function SurfaceView({ ui }: { ui: A2uiPayload }) {
  return (
    <Boundary key={ui.surface_id + ui.messages.length}>
      <Surface ui={ui} />
    </Boundary>
  )
}
