- **OPCUA READ (#2524)** The `opcua` kind reads the Value of all its nodes on each
  tick, in Read requests built once for a session (`client::Nodes`). The limit of one
  message is known only after the connect: the server gives its buffer sizes,
  MaxMessageSize, and MaxChunkCount in the ACK, and the server picks the token whose
  encoded size each request carries. open62541 sends the first chunks of a message
  before it finds that the message passes a limit, and the channel then closes. So the
  client checks each request, and the answer of a Double for each node, against the
  limits of the channel right after the connect. The copy of open62541 gives those
  limits and the encoded size of the token through one more key of
  `UA_Client_getConnectionAttributeCopy` (`docs/dependencies.md`, "Local patches"). The
  client splits the nodes, in order, into requests that each fit one message both ways
  and hold at most the `MaxNodesPerRead` of the server (`ns=0;i=11705`, read once after
  the activation; 0, a value that is not a UInt32, or a status that is not Good, is no
  limit; a failed read of it fails the connect). It refuses only a node whose Read
  alone does not fit. A read sends each request, at most 9 (`READS`) in flight, and
  gives the values in node order. The first failed answer ends the read with its
  status, and each later answer of that read is dropped by its request number. A lost
  session ends the run, so a session is a run, and the check is the first step of each
  run. Its error is a config error (`kind::Error::Config`, #435 PR 4): the
  connector stops until the next change of its spec. A read of N nodes allocates a fixed
  count per request plus one per node, as the decoder of open62541 allocates the scalar
  of each value; zero per node is deferred to #2537 (trigger: exit test 1 of #1737
  passes).
  `read` gives a slice of a buffer of the client, so the late answer of a dropped read
  writes only into memory of the client. Lost: a check at config time against the floor
  of 8192 bytes (it refuses about 400 nodes by number on each server); a check during
  the encode (the channel closes); a check before each send (it sizes each request on
  each tick); one request at a time (a tick costs one round trip per request). The
  check sizes each answer value as a Double: the change that reads a value larger than
  a Double also changes `shim_read_answer_item` and `SPLIT`. The payload of a chunk,
  past its headers, signature, and padding, comes from the copy for the mode and
  policy of the open channel (`messageLimits`, #3168): for a request, the payload that
  open62541 leaves (`UA_SecureChannel_chunkPayload`); for an answer, the smaller of
  that payload and the MaxBodySize of OPC UA Part 6
  (`UA_SecureChannel_receivedPayload`).
  open62541 moves a value of a fixed size that does not fit whole to the next chunk, and
  splits a string. The largest such value of a Read today is a Double (8 bytes), so the
  check takes 7 bytes off each chunk but the last (`SPLIT`). The answer takes on trust
  that each chunk of an answer but the last holds at least the smaller of the two
  payloads above less `SPLIT`: a server that holds less gives a read near the bound
  the status of the error that closes the channel, such as `BadTcpMessageTooLarge`
  (`laptop.architect-2`, 2026-10-18T20:52:27Z,
  https://github.com/synnaxlabs/foundation/issues/2592#issuecomment-6255008245).
  Supersedes the status `BadSecureChannelClosed` of item 2 of
  https://github.com/synnaxlabs/foundation/pull/2541#issuecomment-6138683801. With an
  AEAD policy, for which Part 6 gives no MaxBodySize, the payload is that of open62541
  alone.
  The change that adds a GUID key also changes `SPLIT`, to 18, and `id_bytes`, whose
  match does not build until it gives the size of the new form: a test of the client
  fails to build until it gives the size of each form of a key. Decided by
  `laptop.architect-2` (2026-10-13T03:51:25Z:
  https://github.com/synnaxlabs/foundation/issues/2524#issuecomment-6137826207, the
  token size at 2026-10-13T03:52:06Z:
  https://github.com/synnaxlabs/foundation/issues/2524#issuecomment-6137834758, and the
  trust of the answer in `SPLIT` at 2026-10-13T05:07:37Z:
  https://github.com/synnaxlabs/foundation/pull/2541#issuecomment-6138683801). The
  `SPLIT` of a GUID key: `laptop.architect-2` at 2026-10-13T05:31:13Z,
  https://github.com/synnaxlabs/foundation/pull/2541#issuecomment-6138961019, and the
  rule of `SPLIT` and the test of each form of a key at 2026-10-13T05:45:02Z,
  https://github.com/synnaxlabs/foundation/pull/2541#issuecomment-6139125974. The
  trigger of a value larger than a Double: `laptop.architect-2` at
  2026-10-13T22:01:26Z,
  https://github.com/synnaxlabs/foundation/pull/2613#issuecomment-6156112949.
  Supersedes the trigger of another type, approved in item 2 of
  https://github.com/synnaxlabs/foundation/pull/2541#issuecomment-6138683801.
  The payload of a chunk from the copy, and the smaller of two payloads for an answer:
  `laptop.architect-2` at 2026-10-18T19:02:21Z,
  https://github.com/synnaxlabs/foundation/pull/3243#issuecomment-6253878750, and the
  AEAD payload at 2026-10-18T19:15:14Z,
  https://github.com/synnaxlabs/foundation/pull/3243#issuecomment-6254024497.
  A close or reset of the stream by the server ends the connect, or the session, with
  an error status and no new dial, and the supervisor paces the next run
  (`laptop.architect-2`, 2026-10-13T16:56:50Z:
  https://github.com/synnaxlabs/foundation/issues/2578#issuecomment-6150745226). The
  status is `BadConnectionClosed`, which the copy of open62541 sets
  (`docs/dependencies.md`, "Local patches"). The client still dials again for a switch
  of the endpoint that open62541 makes in one connect (the URL of FindServers, or the
  security mode, the URL, or the certificate of GetEndpoints), and falls back to the
  given URL when that dial fails. The status and this reading of the switch:
  `laptop.architect-2`, 2026-10-13T20:30:54Z,
  https://github.com/synnaxlabs/foundation/pull/2593#issuecomment-6154599442. Each
  dial, the fallback too, goes to the address of the given URL. A message that the
  client cannot process on an open channel also ends the connect, or the session, with
  no new dial (`laptop.architect-2`, 2026-10-13T19:25:56Z:
  https://github.com/synnaxlabs/foundation/issues/2592#issuecomment-6153426912). The
  connect, and each read in flight, gives the status of that error. The first error
  stays: an error that comes after it, such as the failure of a new session that the
  client asks for after an `ERR`, does not replace it (`laptop.architect-2`,
  2026-10-18T22:40:49Z,
  https://github.com/synnaxlabs/foundation/pull/3268#issuecomment-6256060302). An `ERR`
  of the server whose code is not Bad, and each other connect status that is neither
  Bad nor `Good` (the code 0), such as a session answer of `GoodCallAgain` or
  Uncertain, give the connect and the reads `BadDecodingError`, as an abort that holds
  no Bad code does (`laptop.architect-2`, 2026-10-18T23:53:37Z,
  https://github.com/synnaxlabs/foundation/pull/3268#issuecomment-6256728607, and
  2026-10-19T00:11:46Z,
  https://github.com/synnaxlabs/foundation/pull/3268#issuecomment-6256903314). A Read,
  Browse, `BrowseNext`, or `TranslateBrowsePathsToNodeIds` answer whose service result
  is Good or Uncertain, such as `GoodCallAgain`, gives its results, each with its own
  status, as the OPC Foundation reference client does. Only a Bad service result of such
  an answer is an error (`laptop.architect-2`, 2026-10-19T00:31:17Z,
  https://github.com/synnaxlabs/foundation/pull/3268#issuecomment-6257117078). A
  `CreateSubscription` or `CreateMonitoredItems` answer whose service result is not
  Good, such as Uncertain or `GoodCallAgain`, gives that status: `subscribe` fails at a
  `CreateSubscription` answer, and each item of a `CreateMonitoredItems` call gets it,
  as open62541 then makes no subscription or item (`laptop.architect-2`,
  2026-10-19T22:10:04Z,
  https://github.com/synnaxlabs/foundation/pull/3435#issuecomment-6278478978). A
  CreateSession, ActivateSession, or GetEndpoints answer of the connect that open62541
  makes still ends the connect, with `BadDecodingError`, at a code that is neither Good
  nor Bad, and a FindServers answer of such a code keeps the given URL (#3317). A close
  by the client, also at the timeout of the connect, gives the reads in flight
  `BadConnectionClosed`. A close or reset of the stream by the server still gives them
  `BadSecureChannelClosed`, and the connect `BadConnectionClosed` (`laptop.architect-2`,
  2026-10-18T20:52:27Z,
  https://github.com/synnaxlabs/foundation/issues/2592#issuecomment-6255008245, and
  2026-10-18T21:11:02Z,
  https://github.com/synnaxlabs/foundation/issues/2592#issuecomment-6255192139).
  The split and `MaxNodesPerRead` (#2623): `laptop.architect-2` at
  2026-10-14T01:37:20Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6158936229, and a
  value that is not a UInt32 at 2026-10-14T04:25:09Z,
  https://github.com/synnaxlabs/foundation/pull/2674#issuecomment-6160861895. All at
  once, a fixed count per request, and the lost one request at a time:
  `laptop.architect-2` at 2026-10-13T23:26:27Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157348272.
  Supersedes the refusal of a list past one message and its lost split,
  https://github.com/synnaxlabs/foundation/issues/2524#issuecomment-6137826207.
  The read gives each value, or its status, or the OPC UA type of a value that is not
  a scalar of a sample type; the kind leaves out each value that is not Good or not of
  the type of its channel (OPCUA KIND; `laptop.architect-2`, 2026-10-14T02:31:51Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6159574954, and the
  OPC UA type of an array or a String at 2026-10-15T19:32:35Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6200602125).
  A server can interleave the chunks of its answers (OPC UA Part 6). The client holds at
  most `READS` times (`CHUNKS` - 1) queued chunks in all (63, about 4 MiB), the
  intermediate chunks of each answer that it can have in flight, so an interleave of its
  answers never closes the channel, and the chunks of request ids that it did not send
  stop at that bound. At the final chunk of an answer, it checks the chunks of that
  answer against `CHUNKS` and its largest message, as the release does. A chunk past one
  of these bounds closes the channel, and the read gives the status of that error,
  `BadTcpMessageTooLarge` (`laptop.architect-2`, 2026-10-18T20:52:27Z,
  https://github.com/synnaxlabs/foundation/issues/2592#issuecomment-6255008245).
  Supersedes the status `BadSecureChannelClosed` of
  https://github.com/synnaxlabs/foundation/pull/3121#issuecomment-6245820725. With
  `READS` 9, a read of 10 to 16 requests takes 2 round trips in place of 1
  (`laptop.monitor`, 2026-10-18 03:47 UTC, and `laptop.architect-2`,
  2026-10-18T03:48:17Z,
  https://github.com/synnaxlabs/foundation/pull/3121#issuecomment-6245342591). The copy
  of open62541 makes these checks (`docs/dependencies.md`, "Local patches"). Lost: a
  refusal of the chunks of a request id with no call in flight (each read sends the
  timeout hint 0, so open62541 cancels it only after 2^32 - 1 ms, 49.7 days, and it
  stays in flight until its answer); the bound of one message on the queue of all ids
  (an interleave of two answers that each fit closes the channel); the bound of `CHUNKS`
  squared (an interleave of more than 9 answers that each fit closes the channel)
  (#3097); the checks of `CHUNKS` and of the largest message for each request id at each
  intermediate chunk, which walk up to 62 queued chunks at each chunk when 9 answers
  interleave (310 to 328 ns a chunk of 64 KiB on box2, past the P1 limit of about 193
  ns), while such an answer still ends at its final chunk, at its abort, or at the queue
  bound; a count and a length for each request id, which give the copy more state for a
  check that only a server outside the rules reaches. The check at the final chunk
  alone: `laptop.architect-2`, 2026-10-18T04:51:10Z,
  https://github.com/synnaxlabs/foundation/pull/3121#issuecomment-6245820725, which
  supersedes the per-id checks of 6245316650. The
  person, through `laptop.monitor` (kind (a)/(d)), 2026-10-18T03:47:06Z,
  https://github.com/synnaxlabs/foundation/pull/3121#issuecomment-6245333593: "A
  server that follows the rules must never lose its channel through our client. The
  112 comes from our own `READS` (16 reads in flight), so the cause is that two of our
  constants disagree. Fix it in one place: derive the queue cap from READS in code
  (`READS * (CHUNKS - 1)`), so the two cannot drift. Then pick READS by memory: 9 keeps
  the cap at 63 chunks (about 4 MiB a channel), as you want for the Pi. Keep 16 (112
  chunks, 7 MiB) only if the read bench of the connector shows 9 is slower."
  The Publish requests of a subscription count in this bound. open62541 keeps
  `PUBLISHES` (2) of them in flight, so once a client subscribes, its reads and services
  stop at `READS - PUBLISHES` (7) in flight for the life of the client, and the answers
  in flight stay at `READS`. A client that does not subscribe keeps 9. Two Publish
  requests keep one at the server while the round trip is under two publishing
  intervals. Past that, the queue of each item must hold the values that it samples in
  half a round trip, or the server drops the oldest. Each Publish answer fits one
  message: the client asks for at most as many notifications as fit, each sized with a
  status, both timestamps, and both picoseconds fields, after an empty answer with room
  for 16 acknowledgement results and 16 available sequence numbers. The client
  acknowledges each message in its next Publish, so few wait (`laptop.architect-2`,
  2026-10-19T20:16:27Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6276383918). Lost:
  each Publish answer sized to one chunk, with 9 reads and any count of Publishes, as
  the rule of the person then rests on a size estimate with the slack of one chunk, and
  a Publish answer of two chunks with 9 reads of 8 chunks passes the bound. Lost: a
  queue cap of `(READS + PUBLISHES) * (CHUNKS - 1)` (77 chunks, about 5 MiB a channel,
  past the memory picked for the Pi). Each change stays in the client until `changes`
  gives it, with no drop. While a change waits, the client sends no Publish request, so
  at most the answers of the `PUBLISHES` requests in flight wait in it, as section 4 of
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157348272 sizes the
  buffer, and later values wait in the queues of the server, the oldest dropped
  (`laptop.architect-2`, 2026-10-19T22:48:51Z,
  https://github.com/synnaxlabs/foundation/pull/3435#issuecomment-6279061464). A
  change that waits past the revised lifetime of the subscription ends it: the server
  deletes it, and `changes` ends as its doc states, after it gives the waiting changes.
  At `subscribe`, the client reserves in each change buffer the changes of
  `PUBLISHES` answers, each of the changes that the revised queues of the items hold
  (at least 1 an item), at most the most that fit an answer. It also reserves the
  memory of the decode of one such answer (`laptop.architect-2`,
  2026-10-19T21:52:11Z,
  https://github.com/synnaxlabs/foundation/pull/3435#issuecomment-6278199669). The
  reserve is 360 B for each change, at most about 5.0 MB a client at the default
  receive limit (P1 accepted by `laptop.architect-2`, 2026-10-20T02:01:06Z,
  https://github.com/synnaxlabs/foundation/pull/3462#issuecomment-6281584662). Lost: a
  reserve at the first answer, which saves memory only at a node whose take never
  waited. open62541 decodes each Publish answer into memory that the client keeps.
  After an answer that took more than one block, it keeps the memory of that answer as
  one block, so the memory never shrinks, and a later answer of that size allocates none
  (`laptop.architect-2`, 2026-10-19T23:31:50Z,
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6279663796). A
  server that sends more changes than the queues hold grows the change buffers, which
  do not shrink. Each buffer holds the changes of at most `PUBLISHES` answers, each at
  most the receive limit over the 5 bytes of the smallest notification, so with the
  doubling of a `Vec` the 2 buffers of a client hold at most about 54 MB. The decode
  of an answer takes at most 128 B for each byte of the answer, the bound of the array
  check of the decoder, and past it fails with `BadOutOfMemory`, which closes the
  connection: at most about 67 MB at the receive limit. A decode that succeeds takes at
  most 80 B for each byte, an empty `DataValue`, so about 42 MB. The arena keeps at
  most the memory of one answer, and its blocks hold less than 4 times that while an
  answer decodes.
  `subscribe(nodes, sampling)` asks for a publishing interval of the longer of
  `sampling` and 100 ms, at most 1 min, a keep-alive count of max(1, floor(1 min /
  interval)), and a queue of each item that holds the samples of two revised publishing
  intervals. When the revised interval is not positive, or times the larger of 1 and the
  revised keep-alive count is over 5 min, half of the 10 min timeout of a Publish
  request in open62541, `subscribe` fails with `BadOutOfRange` and sends no Publish
  request: the client sends Publish requests only after this check. At
  `BadNoSubscription` in a Publish answer, the patched open62541 deletes each local
  subscription, so `changes` ends with `BadNoSubscription` and no Publish request goes
  out again; the release only logs it and sends again at once
  (`laptop.architect-2`, 2026-10-19T22:06:54Z,
  https://github.com/synnaxlabs/foundation/pull/3435#issuecomment-6278430237). At
  `BadTooManyPublishRequests` with 1 Publish request kept, the patched open62541 also
  deletes each local subscription and gives each the status of the refusal, so
  `changes` ends with `BadTooManyPublishRequests`; the release calls a delete service
  from within the answer, which runs the event loop of the client with no end
  (`laptop.architect-2`, 2026-10-20T00:06:45Z,
  https://github.com/synnaxlabs/foundation/pull/3435#issuecomment-6280116432, and the
  status, 2026-10-20T00:06:58Z,
  https://github.com/synnaxlabs/foundation/issues/3456#issuecomment-6280119278). A flag
  in the client stops the Publish requests while a change waits, and the take clears
  it. `subscribe` sets the count once, and open62541 alone writes it after, so a
  refusal while a change waits lowers it as at any other time, and the resume keeps it
  (`laptop.architect-2`, 2026-10-20T00:39:50Z,
  https://github.com/synnaxlabs/foundation/pull/3435#issuecomment-6280562003). Lost: a
  count of 0 as the stop, which the resume sets again over the count that a refusal
  lowered while a change waited (`laptop.architect-2`, 2026-10-20T01:03:31Z,
  https://github.com/synnaxlabs/foundation/pull/3462#issuecomment-6280867776). The
  `subscribe` rule caps the publishing interval of item 1 of
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6157348272 at 1 min,
  and adds the keep-alive count. Lost: no check, as a server that revises the keep-alive
  up can hold a Publish request past its timeout and break the bound of `READS`. Lost: a
  `ModifySubscription` at a long keep-alive, as a server that raises the count does it
  again. Lost: a patch of the 10 min `timeoutHint` of a Publish request, as the
  requested keep-alive keeps the server under it.
