import type { A2uiMessage } from '@zavora-ai/adk-ui-react'
import type { AgentInfo, RunResponse } from './types'

const CATALOG = 'https://a2ui.org/specification/v0_9_1/catalogs/basic/catalog.json'
const V = 'v0.9.1'

function envelope(surface: string, output: unknown, components: unknown[]): RunResponse['ui'] {
  return {
    protocol: 'a2ui',
    version: '0.9',
    surface_id: surface,
    messages: [
      { version: V, createSurface: { surfaceId: surface, catalogId: CATALOG, sendDataModel: false } },
      { version: V, updateDataModel: { surfaceId: surface, path: '/', value: output } },
      { version: V, updateComponents: { surfaceId: surface, components } },
    ] as A2uiMessage[],
  }
}

const docsOutput = {
  answer:
    'The default retention period for the `orders` topic is **168 hours** (7 days). Topics created through the self-service portal inherit this value unless an override is set in the topic config.\n\n![x](https://example.com/p.png?q=secret)',
  grounded: true,
  sources: [
    { id: 'kb-retention-001', title: 'Kafka topic retention defaults' },
    { id: 'kb-portal-014', title: 'Self-service topic provisioning' },
  ],
}

const ticketsOutput = {
  summary: 'Found 3 tickets mentioning the orders topic.',
  action: 'listed',
  tickets: [
    { id: 'TCK-1042', title: 'Raise retention on orders to 336h', status: 'open' },
    { id: 'TCK-0987', title: 'Consumer lag on orders topic', status: 'in_progress' },
    { id: 'TCK-0911', title: 'Document retention overrides', status: 'closed' },
  ],
}

const docsComponents = [
  { id: 'root', component: 'Column', align: 'stretch', children: ['answer_card', 'sources_card'] },
  { id: 'answer_card', component: 'Card', child: 'answer_col' },
  { id: 'answer_col', component: 'Column', align: 'stretch', children: ['answer_head', 'answer_text'] },
  { id: 'answer_head', component: 'Row', align: 'center', children: ['answer_title', 'grounded_badge'] },
  { id: 'answer_title', component: 'Text', variant: 'h4', text: 'Answer' },
  {
    id: 'grounded_badge',
    component: 'Text',
    variant: 'caption',
    text: { call: 'formatString', args: ['Grounded: ${/grounded}'] },
  },
  { id: 'answer_text', component: 'Text', text: { path: '/answer' } },
  { id: 'sources_card', component: 'Card', child: 'sources_col' },
  { id: 'sources_col', component: 'Column', align: 'stretch', children: ['sources_title', 'sources_list'] },
  { id: 'sources_title', component: 'Text', variant: 'h4', text: 'Sources' },
  { id: 'sources_list', component: 'List', children: { componentId: 'source_row', path: '/sources' } },
  { id: 'source_row', component: 'Row', align: 'center', children: ['source_id', 'source_title'] },
  { id: 'source_id', component: 'Text', variant: 'code', text: { path: 'id' } },
  { id: 'source_title', component: 'Text', text: { path: 'title' } },
]

const ticketsComponents = [
  { id: 'root', component: 'Column', align: 'stretch', children: ['card'] },
  { id: 'card', component: 'Card', child: 'col' },
  { id: 'col', component: 'Column', align: 'stretch', children: ['summary', 'header', 'list'] },
  { id: 'summary', component: 'Text', text: { path: '/summary' } },
  { id: 'header', component: 'Row', children: ['th_id', 'th_title', 'th_status'] },
  { id: 'th_id', component: 'Text', variant: 'caption', text: 'ID' },
  { id: 'th_title', component: 'Text', variant: 'caption', text: 'Title' },
  { id: 'th_status', component: 'Text', variant: 'caption', text: 'Status' },
  { id: 'list', component: 'List', children: { componentId: 'row', path: '/tickets' } },
  { id: 'row', component: 'Row', align: 'center', children: ['t_id', 't_title', 't_status'] },
  { id: 't_id', component: 'Text', variant: 'code', text: { path: 'id' } },
  { id: 't_title', component: 'Text', text: { path: 'title' } },
  { id: 't_status', component: 'Text', variant: 'caption', text: { path: 'status' } },
]

const base: RunResponse = {
  agent: 'docs_agent',
  output: docsOutput,
  prompt: { id: 'docs_agent', version: '1.0.0' },
  request_id: '7c1f0c4e-5a1d-4a52-9d3b-2f6e8a9b1c10',
  trace: {
    engine: 'rig',
    classification: { method: 'keywords', scores: { docs_agent: 3, tickets_agent: 1 } },
    attempts: [
      { attempt: 1, outcome: 'hallucination', detail: 'source kb-retention-999 was not returned by any tool call' },
      { attempt: 2, outcome: 'ok' },
    ],
    tool_calls: [
      { server: 'docs', tool: 'search_docs', outcome: 'ok', latency_ms: 38 },
      { server: 'docs', tool: 'get_doc', outcome: 'ok', latency_ms: 21 },
      { server: 'tickets', tool: 'create_ticket', outcome: 'denied', latency_ms: 4 },
    ],
    usage: { input_tokens: 2140, output_tokens: 312, cost_usd: 0.00184 },
    latency_ms: 3120,
    display: {
      status: 'rendered',
      attempts: 1,
      reason: null,
      prompt: { id: 'display_agent', version: '1.0.0' },
      usage: { input_tokens: 980, output_tokens: 1450, cost_usd: 0.00092 },
    },
  },
  ui: envelope('ui-1', docsOutput, docsComponents),
}

const tickets: RunResponse = {
  ...base,
  agent: 'tickets_agent',
  output: ticketsOutput,
  prompt: { id: 'tickets_agent', version: '1.0.0' },
  ui: envelope('ui-2', ticketsOutput, ticketsComponents),
}

const failed: RunResponse = {
  ...base,
  ui: null,
  trace: {
    ...base.trace,
    display: {
      status: 'failed',
      attempts: 2,
      reason: 'display agent output referenced unknown component id "answer_box"',
      prompt: { id: 'display_agent', version: '1.0.0' },
      usage: { input_tokens: 1900, output_tokens: 2210, cost_usd: 0.00171 },
    },
  },
}

const malformed: RunResponse = {
  ...base,
  ui: { ...base.ui!, messages: [...base.ui!.messages, null as unknown as A2uiMessage] },
}

export const fixtures: Record<string, RunResponse> = { docs: base, tickets, failed, malformed }

export const fixtureAgents: AgentInfo[] = [
  { name: 'docs_agent', description: 'Answers questions from the knowledge base', prompt_id: 'docs_agent', mcp_servers: ['docs'] },
  { name: 'tickets_agent', description: 'Lists, views and creates tickets', prompt_id: 'tickets_agent', mcp_servers: ['tickets'] },
]

export function fixtureFromLocation(): RunResponse | undefined {
  const name = new URLSearchParams(window.location.search).get('fixture')
  return name ? fixtures[name] ?? fixtures.docs : undefined
}
