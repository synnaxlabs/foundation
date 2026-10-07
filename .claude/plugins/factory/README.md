# factory

A Claude Code mod that lets the factory's sessions message each other over AWS IoT
Core. A message starts a turn in the receiver; the receiver answers with the `send`
tool. Tested with Claude Code 2.1.292 (`claude --version`).

## Start a session

```sh
~/.factory/bin/factory-agent <machine>.<role>
```

Every machine has this script. It sources `~/.factory/env`, makes the role's worktree,
and runs `FACTORY_NAME=<name> claude ... --plugin-dir <plugin checkout>`. The plugin
checkout is `~/Desktop/synnaxlabs/foundation-wt/factory-plugin`. Each new session
moves it to the latest `main`; when the mod changed, the running sessions on that
machine reload it.

`FACTORY_NAME` must be in `roster.json`, which also holds the broker host. When it is
not set or not in the roster, the mod is off and the status line says why.

## Broker

Each machine has `~/.factory/iot/{root-ca.pem,cert.pem,key.pem}`, and the certificate
CN is the machine (`laptop`, `box1`, `box2`). The IoT policy accepts a client ID and
the sender level of a topic only when they start with `<CN>.`. A publish that breaks
the policy drops the connection.

The topics:

- `factory/<to>/inbox/<from>` -> a message, QoS 1: `{ id, text, sentAt }`. The
  receiver takes the sender from the topic, never from the payload.
- `factory/<to>/acks/<from>` -> `{ id }`, QoS 1, when the receiver has stored the
  message. Each session also sends a QoS 0 probe to its own acks topic each minute:
  the link is up while the probe comes back.
- `factory/metrics/<name>` -> QoS 0, for the monitor:
  `{ name, at, usage, recv, sent, queued, noAcks, capped }`, with `usage` from
  `$.session.usage()`. Sent when it changes, at most once a minute.

Each session runs one `mosquitto_sub` with client ID `<name>` and a persistent session
(`-c`), so the broker keeps messages while the session is closed. Each publish uses
its own client ID, `<name>.p<random>`: a second connection with ID `<name>` would
disconnect the subscriber.

## Behavior

- Messages wait in `$.store`, under the session's name, until their turn starts. A
  restarted session starts them.
- A repeated message id gets an ack and no second turn.
- When a message gets no ack in 60 s, the sender gets one notice turn for that
  receiver, and no other until the receiver acks again.
- One turn takes all the messages waiting when the session goes idle. A message's later
  lines are indented two spaces, so no line passes for a header.
- Messages start at most 30 turns an hour. Past that they wait, and the status line
  says so.
- The status line shows the name, the link, messages in and out, the queue, the cost,
  and the rate limits.

## Check it

```sh
claude plugin validate --strict .claude/plugins/factory
claude plugin test .claude/plugins/factory
```
