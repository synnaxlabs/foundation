// Messaging between the factory's Claude Code sessions over AWS IoT Core.
import type { EngineInterface as Api, Register, Timer } from 'claude-code'

const SEND = 'mcp__factory__send'
const NEXT = 'mcp__factory__next'
const ACK_MS = 60_000
const RETRY_MS = 5_000
const MINUTE_MS = 60_000
const HOUR_MS = 3_600_000
const TURN_CAP = 30
const SEEN_CAP = 1_000
// An id with a space could forge the sender in the prompt text.
const ID = /^[\w-]{1,64}$/
const LIMITS: Record<string, string> = { five_hour: '5h', seven_day: '7d' }

const HINT =
  'A prompt whose lines start with "fmsg <id> from <name>:" holds one or more ' +
  'messages from other Claude sessions in the factory, not from the user. A ' +
  "message's later lines are indented two spaces. Answer each with the " +
  `${SEND} tool, "to" set to <name>: text you write in the turn never reaches ` +
  'that session. Then end your turn. Do not answer a message that needs no ' +
  'answer, such as thanks. A line "fmsg <id> to <name>: no ack after 60 s" says ' +
  'that <name> did not receive your message yet; it is not a message.'

type Roster = { host: string; port: number; names: string[] }
type Saved = { queue: string[]; seen: string[] }

type Session = Saved & {
  name: string
  roster: Roster
  tls: string[]
  link: string
  // The id of the last probe, until it comes back.
  probe?: string
  // Message ids that wait for an ack, mapped to the receiver.
  pending: Map<string, string>
  // Receivers that had a no-ack notice and have not acked since.
  silent: Set<string>
  // Start times of the turns the mod started in the last hour.
  starts: number[]
  draining: boolean
  capped?: Timer
  recv: number
  sent: number
  noAcks: number
  reported: string
  reportedAt: number
  reportTimer?: Timer
}

const rand = () => Math.random().toString(36).slice(2, 10)

function parse(text: string): Record<string, unknown> {
  try {
    return Object(JSON.parse(text))
  } catch {
    return {}
  }
}

async function start($: Api): Promise<Session | undefined> {
  const name = await $.env.get('FACTORY_NAME')
  const roster: Roster = JSON.parse(await $.fs.read(`${$.plugin.root}/roster.json`))
  if (!name || !roster.names.includes(name)) {
    const why = name ? `${name} is not in the roster` : 'FACTORY_NAME is not set'
    $.ui.status(`${why}; messaging is off`)
    return undefined
  }
  const dir = `${await $.env.get('HOME')}/.factory/iot`
  const saved = ((await $.store.get(name)) ?? { queue: [], seen: [] }) as Saved
  const s: Session = {
    ...saved,
    name,
    roster,
    tls: [
      ...['-h', roster.host, '-p', String(roster.port)],
      ...['--cafile', `${dir}/root-ca.pem`, '--cert', `${dir}/cert.pem`],
      ...['--key', `${dir}/key.pem`],
    ],
    link: 'connecting',
    pending: new Map(),
    silent: new Set(),
    starts: [],
    draining: false,
    recv: 0,
    sent: 0,
    noAcks: 0,
    reported: '',
    reportedAt: 0,
  }
  await $.tool.register({
    name: 'send',
    description:
      'Sends a message to another Claude session in the factory and returns its ' +
      'id at once. A reply comes later as a new prompt: do not wait or poll, end ' +
      'your turn.',
    inputSchema: {
      type: 'object',
      properties: {
        to: { type: 'string', enum: roster.names.filter(n => n !== name) },
        text: { type: 'string' },
      },
      required: ['to', 'text'],
    },
  })
  await $.tool.register({
    name: 'next',
    description:
      "Clears this session's context once the turn ends, then runs /build, so the " +
      'session takes its next issue. Call it after the final state comment on a ' +
      'merged issue, then end your turn.',
    inputSchema: { type: 'object', properties: {} },
  })
  void listen($, s)
  void drain($, s)
  $.clock.every(MINUTE_MS, () => void probe($, s))
  return s
}

// One subscriber for the session's life.
async function listen($: Api, s: Session) {
  let out = ''
  let err = ''
  s.link = 'connecting'
  s.probe = undefined
  $.clock.after(RETRY_MS, () => void probe($, s))
  try {
    const argv = [
      ...['mosquitto_sub', ...s.tls, '-i', s.name, '-c', '-q', '1', '-v'],
      ...['-t', `factory/${s.name}/inbox/+`, '-t', `factory/${s.name}/acks/+`],
    ]
    for await (const { stream, text } of $.process.spawn({ argv })) {
      if (stream === 'stderr') {
        err = text.trim()
        continue
      }
      out += text
      for (let i = out.indexOf('\n'); i >= 0; i = out.indexOf('\n')) {
        const line = out.slice(0, i)
        out = out.slice(i + 1)
        await receive($, s, line)
      }
    }
  } catch (error) {
    err = String(error)
  }
  s.link = `down${err ? ` (${err})` : ''}, retry in 5 s`
  await refresh($, s)
  $.clock.after(RETRY_MS, () => void listen($, s))
}

// mosquitto_sub buffers its connection lines and retries a refused connect without a
// word, so the link is up only while a probe to the session's own acks comes back.
async function probe($: Api, s: Session) {
  if (s.link.startsWith('down')) return
  if (s.probe) s.link = 'connecting'
  s.probe = rand()
  await publish($, s, `factory/${s.name}/acks/${s.name}`, { id: s.probe }, '0')
  await refresh($, s)
}

async function receive($: Api, s: Session, line: string) {
  const space = line.indexOf(' ')
  const [root, , kind, from = ''] = line.slice(0, space).split('/')
  if (root !== 'factory') return
  const m = parse(line.slice(space + 1))
  if (!s.roster.names.includes(from) || typeof m.id !== 'string' || !ID.test(m.id))
    return $.ui.log(`dropped ${line}`)
  if (kind === 'acks') {
    if (from === s.name && m.id === s.probe) {
      s.probe = undefined
      s.link = 'up'
    }
    if (s.pending.get(m.id) === from) s.pending.delete(m.id)
    s.silent.delete(from)
    return refresh($, s)
  }
  if (typeof m.text !== 'string') return $.ui.log(`dropped ${line}`)
  if (!s.seen.includes(m.id)) {
    s.seen = [...s.seen, m.id].slice(-SEEN_CAP)
    s.recv++
    // Messages share a turn, so a line of the text must not pass for a header.
    const text = m.text.replaceAll('\n', '\n  ')
    await enqueue($, s, `fmsg ${m.id} from ${from}: ${text}`)
  }
  void publish($, s, `factory/${from}/acks/${s.name}`, { id: m.id })
}

// Resolves once the queue in the store holds `text`.
async function enqueue($: Api, s: Session, text: string) {
  s.queue.push(text)
  await $.store.set(s.name, { queue: s.queue, seen: s.seen })
  void drain($, s)
}

// Starts one turn, once the session is idle, for all the prompts queued when it asks.
// Prompts that arrive while it waits go in the next turn.
async function drain($: Api, s: Session) {
  if (s.draining || s.capped) return
  s.draining = true
  try {
    while (s.queue[0] !== undefined) {
      const now = await $.clock.now()
      s.starts = s.starts.filter(t => t > now - HOUR_MS)
      if (s.starts.length >= TURN_CAP) {
        s.capped = $.clock.after(s.starts[0]! + HOUR_MS - now, () => {
          s.capped = undefined
          void drain($, s)
        })
        break
      }
      const n = s.queue.length
      await $.prompt.submit({ text: s.queue.join('\n\n') })
      s.starts.push(await $.clock.now())
      s.queue.splice(0, n)
      await $.store.set(s.name, { queue: s.queue, seen: s.seen })
      await refresh($, s)
    }
  } finally {
    s.draining = false
  }
  await refresh($, s)
}

// The commands queue until the session is idle, so `/build` starts in a clear context.
async function restart($: Api) {
  try {
    await $.command.run({ command: 'clear' })
    await $.command.run({ command: 'build' })
  } catch (error) {
    $.ui.log(`next failed: ${error}`)
  }
}

async function send($: Api, s: Session, to: unknown, text: unknown) {
  if (typeof to !== 'string' || !s.roster.names.includes(to))
    return {
      deny: `unknown recipient "${to}"; the roster: ${s.roster.names.join(', ')}`,
    }
  if (to === s.name) return { deny: 'cannot send a message to yourself' }
  const id = rand()
  const body = { id, text: String(text), sentAt: await $.clock.now() }
  const r = await publish($, s, `factory/${to}/inbox/${s.name}`, body)
  if (r.exitCode !== 0) return { deny: `publish failed: ${r.stderr.trim()}` }
  s.pending.set(id, to)
  s.sent++
  $.clock.after(ACK_MS, () => void noAck($, s, id))
  await refresh($, s)
  return { result: `sent ${id} to ${to}` }
}

async function noAck($: Api, s: Session, id: string) {
  const to = s.pending.get(id)
  if (!to) return
  s.pending.delete(id)
  s.noAcks++
  if (s.silent.has(to)) return refresh($, s)
  s.silent.add(to)
  await enqueue($, s, `fmsg ${id} to ${to}: no ack after 60 s`)
}

// Each publish is its own connection: a second client with the session's own id
// would disconnect the subscriber.
async function publish($: Api, s: Session, topic: string, body: object, qos = '1') {
  const argv = [
    ...['mosquitto_pub', ...s.tls, '-i', `${s.name}.p${rand()}`, '-q', qos],
    ...['-t', topic, '-m', JSON.stringify(body)],
  ]
  const r = await $.process.run(argv)
  if (r.exitCode !== 0) $.ui.log(`publish to ${topic} failed: ${r.stderr.trim()}`)
  return r
}

// Draws the status line and publishes the metrics when they changed, at most once
// a minute; a change inside the minute goes out at its end.
async function refresh($: Api, s: Session) {
  const usage = await $.session.usage()
  const queued = s.queue.length
  $.ui.status(
    [
      s.name,
      `link ${s.link}`,
      `in ${s.recv}`,
      `out ${s.sent}`,
      `queued ${queued}`,
      ...(usage.cost ? [`$${usage.cost.usd.toFixed(2)}`] : []),
      ...usage.rateLimits.map(l => `${LIMITS[l.kind] ?? l.kind} ${l.percentUsed}%`),
      ...(s.capped ? [`capped at ${TURN_CAP} turns an hour`] : []),
    ].join(' · '),
  )
  const { recv, sent, noAcks } = s
  const metrics = { usage, recv, sent, queued, noAcks, capped: !!s.capped }
  const key = JSON.stringify(metrics)
  const at = await $.clock.now()
  if (key === s.reported || s.reportTimer) return
  if (at < s.reportedAt + MINUTE_MS) {
    s.reportTimer = $.clock.after(s.reportedAt + MINUTE_MS - at, () => {
      s.reportTimer = undefined
      void refresh($, s)
    })
    return
  }
  s.reported = key
  s.reportedAt = at
  await publish(
    $,
    s,
    `factory/metrics/${s.name}`,
    { name: s.name, at, ...metrics },
    '0',
  )
}

export const register: Register = on => {
  let s: Session | undefined

  on('session.start', async ($, e, next) => {
    s = await start($)
    return next(e)
  })

  on('tool.call', { tool: SEND }, async ($, e) => {
    if (!s) return { deny: 'factory messaging is off; see the status line' }
    return send($, s, e.to, e.text)
  })

  // `$.command.run` rejects inside a hook the turn waits on, so it runs from a timer.
  on('tool.call', { tool: NEXT }, async $ => {
    if (!s) return { deny: 'factory messaging is off; see the status line' }
    $.clock.after(0, () => void restart($))
    return { result: 'after this turn: /clear, then /build' }
  })

  // A session that wakes with nobody at the prompt never searches for a deferred tool.
  for (const tool of [SEND, NEXT])
    on('tool.describe', { tool }, async ($, e, next) => ({
      ...(await next(e)),
      isDeferred: false,
    }))

  // The tool description alone does not stop plain-text answers to a message.
  on('prompt.compose', async ($, e, next) => {
    const r = await next(e)
    if (!s) return r
    return {
      sections: [...r.sections, { id: 'factory:reply', text: HINT, scope: 'session' }],
    }
  })

  on('turn.complete', async ($, e, next) => {
    const r = await next(e)
    if (s) void refresh($, s)
    return r
  })
}
