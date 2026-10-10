- **OPEN62541 SOURCE (#435)** We copy the upstream source files of open62541, not the
  amalgamation: the amalgamation adds the POSIX clock and event loop even with
  `UA_ARCHITECTURE=none`. The 3 global clock functions give a fixed time. That is
  acceptable only with a closed list of the (file, enclosing function) pairs that may
  call one; a list per file would pass a new call in a listed file. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050922367,
  2026-10-08 02:27 UTC). The copy goes in `patches/open62541/`, and the `build.rs` of
  `connector-opcua` reads its `sources.txt`. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057244538,
  2026-10-08 09:50 UTC). `cargo xtask open62541 <tag>` makes the copy. Each file from
  the release is byte for byte the file at the tag. Every other file (`src_generated/`,
  `sources.txt`, `flags.txt`, `VERSION`) is the output of that command alone, never
  edited by hand. Our change edits only release files. `cargo xtask open62541` is the
  clock check. It builds the copy from its own files with `-g -O0` and reads the call
  relocations against the closed list. It fails on a call outside the list, a listed
  pair with no call, a clock address in any section that is not code, each
  `DW_TAG_inlined_subroutine`, and a header outside the copy. It runs on the staged
  copy before `<tag>` replaces anything, and on the committed copy with no tag. A test
  in `cargo test -p xtask` runs it on the committed copy. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057554572,
  2026-10-08 10:08 UTC). PR 2 of #435 adds that test with the copy. Approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058025302,
  2026-10-08 10:37 UTC). #1860 makes CI run it on a PR that changes only `patches/`.
  The check reads the undefined symbols of each object (`nm -u`) and fails on each
  symbol outside the copy that is not on a closed list, with the file and the symbol.
  `SYMBOLS` admits a symbol for any file, and `FILE_SYMBOLS` admits a (file, symbol)
  pair, such as a call of the stdout logger, which we never run. Each entry has its
  reason. A symbol goes in `SYMBOLS` only when it reads no clock, file, network,
  randomness, or process state: memory and string functions, and the 5 constructors that
  `shim.c` defines to abort. Three exceptions read process state: the allocator, whose
  addresses the OS places at random, so no result of the copy may depend on an address;
  `errno`, which the copy reads only for the error of its own call; and the table of the
  linker. The 3 clock functions pass this check, because the clock check reads each
  reference to one. The check builds with no stack protector, so the compiler adds no
  reference to its random canary, and a reference that the C makes fails. A symbol is
  outside the copy when no object exports it: a `static` function of one file does not
  hide a call of the OS function of its name from another. A pair of `FILE_SYMBOLS` with
  no reference fails, so a file that the build leaves out loses its pairs. `OUTSIDE` in
  `connector-opcua` lists the outside symbols of the production build, so a new outside
  symbol changes both lists. A header list is not a check: a listed header can include
  another (`pthread.h` includes `time.h`). So the check refuses no system header, and
  each header that the copy includes must be in the copy or in a system directory as
  `cc` finds it. Lost: a header list, and a deny list of OS symbols, which passes a call
  that it does not name. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1884#issuecomment-6060989375,
  2026-10-08 13:31 UTC). Supersedes the closed list of system headers of
  https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058446715. The
  check fails on every reference to a clock function that is not a call, also one in
  code. A call relocation counts as a call only in a section that `objdump -d`
  disassembles. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058446715,
  2026-10-08 11:03 UTC). Supersedes the clock address rule of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057554572. The
  check stands against a clock reference that the compiler makes from C in the copy,
  from a new tag or from our patch. It does not stand against an edit made to hide
  from it, such as assembly that stores a function's address: review of each copy PR
  covers that. A `#line` directive or a line marker in a copy file fails the check,
  because it moves the file that the include check reads. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058103514,
  2026-10-08 10:42 UTC). No flag of the upstream build is lost with no error. Decided
  by `laptop.director`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6060613260,
  2026-10-08 13:10 UTC). `cargo xtask open62541` puts each flag of a compile in
  `flags.txt` (`-D`, `-I`, `-std`, and `CODE_FLAGS`, the flags that change the code)
  or in `LEFT_OUT`, a closed list with the reason of each, and fails on any other
  flag. `build.rs` and the check both read `flags.txt`, so the check reads objects
  compiled with the flags of the connector. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6060989849,
  2026-10-08 13:31 UTC). The `UA_ARCH_HEADER` of the allocator below and the sanitizer
  flags below are the only flags of `build.rs` outside `flags.txt`, so the objects of
  the check do not have them. The check adds only `-g -O0` and `-fno-stack-protector`
  (above). A `-W` flag with no `,` is a warning, which changes no code, so `collect`
  leaves it out by that pattern, not by name. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1893#issuecomment-6061473044,
  2026-10-08 13:57 UTC) and `laptop.director`
  (https://github.com/synnaxlabs/foundation/pull/1893#issuecomment-6061540779,
  2026-10-08 14:00 UTC). Supersedes, for `-W` flags, the closed list of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6060613260 and of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6060989849
  (`laptop.director`,
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6064080980,
  2026-10-08 16:10 UTC).
  Our change makes the random state `UA_rng` of `src/util/ua_util.c` one per thread
  (`UA_THREAD_LOCAL`), so a draw on one thread does not move the state of another.
  Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050889018,
  2026-10-08 02:24 UTC). A test in `cargo test -p xtask` links the objects of the
  check with a C driver in `xtask/`: the main thread sets the start value 1, joins a
  thread that sets 2 and draws, then draws, and its values must equal those of a
  thread that sets 1 alone. The driver defines each clock function to call `abort()`,
  and the test asserts its exact output. The end-to-end check of PR 4 of #435 covers
  the production build. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6059441203,
  2026-10-08 12:03 UTC). A draw on a thread with no start value aborts
  (https://github.com/synnaxlabs/foundation/pull/1909#issuecomment-6064798117,
  2026-10-08 16:51 UTC). Nothing in the library sets a start value, also in
  production. So `connector-opcua` (PR 4 of #435) sets the start value
  with `UA_random_seed_deterministic`, taken from the randomness of `env`, and never
  calls `UA_random_seed`, which reads the clock. It does so on each thread before that
  thread calls open62541, and runs each server and each client on one thread. Its
  test server sets the start value of the test at start, and its end-to-end check
  asserts the same run for the same value. A state for each `UA_Server` and
  `UA_Client` lost: the draw functions and the security policy plugins take no
  server, so each call site changes, and LOCAL PATCHES does that work again at each
  release. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1906#issuecomment-6063691059,
  2026-10-08 15:49 UTC).
  The generator is PCG32 (`UA_ENABLE_DETERMINISTIC_RNG`), which a peer can predict from
  a few of its values. Foundation code takes no nonce, key, or session token from
  `UA_UInt32_random` or `UA_Guid_random`. The copy does so in two places: the nonces of
  the security policy None, which protect nothing, and the session authentication token
  of its server, which only the test server of `connector-opcua` runs. Before Foundation
  builds a security policy that encrypts or an OPC UA server, that work takes each
  nonce, key, and token from a cryptographic source, and its PR records the source here.
  Decided by `laptop.architect-2`, 2026-10-09 13:10 UTC
  (https://github.com/synnaxlabs/foundation/pull/1997#issuecomment-6081545452).
  A second change of `src/util/ua_util.c` keeps a flag for each thread, which
  `UA_random_seed` and `UA_random_seed_deterministic` set, and `UA_UInt32_random` and
  `UA_Guid_random` call `abort()` on a thread with no start value. The C driver then
  calls each of the two draws on a thread with none, and the test asserts the abort
  and its exact output. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1909#issuecomment-6064798117,
  2026-10-08 16:51 UTC). Supersedes the record text "each thread with none draws the
  same fixed values", which cited
  https://github.com/synnaxlabs/foundation/pull/1906#issuecomment-6063691059. The
  line of `UA_random_seed` that sets the flag has no test: no path of our build
  reaches it, and a test needs a driver whose clock does not abort. Approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6065760073,
  2026-10-08 17:47 UTC). This change and its driver test ship in a PR of their own,
  apart from the connector code (same comment).
  The feature `open62541` of `connector-opcua` compiles the copy and `src/shim.c`
  with `cc`. `shim.c` defines the 8 symbols that the copy leaves undefined: the 3
  clock functions give 0, and the 5 POSIX constructors print their name and abort,
  since our config always has an event loop. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050922367,
  2026-10-08 02:27 UTC). A test asserts the pragmas of `shim.c` after the
  preprocessor. It stands against honest code. A `#line` directive or a line marker
  in `shim.c` has no honest use, since the file is written by hand, so review of
  `shim.c` covers it, and the `#line` rule of the copy check does not apply to it. If
  `shim.c` is ever generated, that rule applies to it. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1947#issuecomment-6067031461,
  2026-10-08 19:03 UTC; the line marker:
  https://github.com/synnaxlabs/foundation/pull/1947#issuecomment-6067179092,
  2026-10-08 19:12 UTC).
  The copy also holds `arch/common/timer.c` and `timer.h`, which the build with
  `UA_ARCHITECTURE=none` does not compile. `cargo xtask open62541` takes them from a
  closed list of extra release files, with the reason of each, and compiles them with
  the flags of `flags.txt`. The event loop of `connector-opcua` holds a `UA_Timer` and
  gives it the time of `env`. Lost: timers in Rust, a copy of library code that already
  takes the time as an input. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6065760073,
  2026-10-08 17:47 UTC; the release path,
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6066098798,
  2026-10-08 18:08 UTC).
  The copy and `shim.c` allocate through the global allocator of the binary.
  `src/alloc.h` is the `UA_ARCH_HEADER` of both builds: it declares the 4 functions
  of `src/alloc.rs` and defines `UA_malloc`, `UA_calloc`, `UA_realloc`, and `UA_free`
  as them, before `config.h` sets the libc calls as the defaults. They keep the C
  contract on `std::alloc`: a failure gives NULL and never panics, `malloc(0)` and
  `realloc(p, 0)` give a unique pointer that is not NULL, and each pointer is aligned
  to 16. No pointer crosses between the libc allocator and these: the objects of the
  copy and of `shim.c` call no libc function that gives or takes a heap pointer. A
  crypto library that a later PR links keeps its own allocator. So the counting
  allocator of a test or benchmark binary counts C too. The copy check compiles
  without `alloc.h`. A test reads the archive that `build.rs` makes with it, and
  fails on each symbol outside the copy and `shim.c` that its closed list does not
  hold. The list holds no clock function and no libc function that gives or takes a
  heap pointer. Lost: `UA_ENABLE_MALLOC_SINGLETON` (a global), `--wrap=malloc`
  (`std::alloc` calls `malloc`, so it recurses), and 4 `-D` flags (a define gives no
  prototype, and C99 needs one). Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6067206932,
  2026-10-08 19:13 UTC; the header:
  https://github.com/synnaxlabs/foundation/pull/1981#issuecomment-6067771921,
  2026-10-08 19:46 UTC). The exception for `UA_ARCH_HEADER` in the flags passage
  above, the copy check without `alloc.h`, and the test of the archive: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1981#issuecomment-6068060133,
  2026-10-08 20:04 UTC, and at 1a593331:
  https://github.com/synnaxlabs/foundation/pull/1981#issuecomment-6069664332,
  2026-10-08 21:46 UTC, and the list for 64-bit Arm at 586e8089:
  https://github.com/synnaxlabs/foundation/pull/1981#issuecomment-6069858320,
  2026-10-08 22:00 UTC).
  `build.rs` gives both builds the sanitizers of the Rust build. With
  `sanitize="address"`, it adds `-fsanitize=address,undefined -fno-sanitize=function
  -fno-sanitize-recover=all` and sets `cfg(asan)`. With `cfg(fuzzing)`, it adds
  `-fsanitize=fuzzer-no-link`. With either, a compiler that is not clang gives way to
  `clang`, since rustc links the LLVM runtimes. The `function` check is off because the
  copy calls functions through a generic type by design (`ZIP_FUNCTIONS`, the
  `UA_Callback` casts). With a sanitizer other than `address` and `leak`, `build.rs`
  fails: the C has no MSan or TSan instrumentation, so MSan reports false errors and
  TSan misses the C. Lost: a Rust MSan or TSan run of this crate (`laptop.architect-2`,
  https://github.com/synnaxlabs/foundation/pull/2165#issuecomment-6088153688, 2026-10-09
  19:51 UTC). Under `cfg(asan)`, the test of the archive also accepts the names of the
  sanitizer runtimes (`__asan_`, `__ubsan_`, `__start_asan_globals`,
  `__stop_asan_globals`). Lost: `CC` and `CFLAGS` set by each job, which gives two
  places that pick the flags, and C with no coverage under a plain `cargo fuzz`; and an
  ignore list for the copy in place of `-fno-sanitize=function`, because the one
  indirect call of `shim.c` that the check reads runs the cast callbacks of the copy.
  The flags follow `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050889018,
  2026-10-08 02:24 UTC, and the `function` check, 2026-10-09 17:35 UTC:
  https://github.com/synnaxlabs/foundation/pull/2165#issuecomment-6086019787).
  Supersedes, for the `function` check, the C flags of rule 1 of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050889018.
  The copy builds with `UA_MULTITHREADING` 0, so it takes no `UA_LOCK` and links no
  `pthread_mutex_*` symbol: each server and each client runs on one thread. Level 0
  alone makes `UA_THREAD_LOCAL` empty, so two threads would share `UA_rng` and the
  `static UA_THREAD_LOCAL` buffers of `src/client/ua_client.c` and the server files.
  So our change of `src_generated/open62541/config.h` moves its thread-local block out
  of `#if UA_MULTITHREADING >= 100`. The driver test of `UA_rng` is its positive
  control, and the test of the archive fails on each `pthread_mutex_*` symbol: the
  closed list holds none. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6068041912,
  2026-10-08 20:03 UTC). Supersedes the note "a copy config with `UA_MULTITHREADING`
  0 is the fix, as its own change" of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6067067211.
  It is the one file outside the release that we edit by hand. Supersedes, for this
  block, "never edited by hand. Our change (PR 3, `UA_rng`) edits only release files"
  of https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057554572. This
  text approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/1995#issuecomment-6069607884,
  2026-10-08 21:42 UTC) and `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1995#issuecomment-6069329793,
  2026-10-08 21:24 UTC).
  A second change of that file adds a branch that sets `UA_FLOAT_LITTLE_ENDIAN` when
  the target is 64-bit Arm and `__BYTE_ORDER__` is little-endian. Clang defines no
  `__FLOAT_WORD_ORDER__`, so without it a Clang build for 64-bit Arm encodes each float
  on the slow path of `pack754`, which links the `long double` helpers. On 64-bit Arm
  the float order is the byte order. The test of `connector-opcua` that preprocesses
  `config.h` for each target is its check. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1995#issuecomment-6071050895,
  2026-10-08 23:26 UTC).
  The event loop of `connector-opcua` is a `UA_EventLoop` that `shim.c` fills and
  `event::Loop` owns, on one thread. Its monotonic time is the clock of `env`.
  `dateTime_now` gives that time counted from the Unix epoch, and the UTC offset is 0,
  until #1992 gives it the wall time of the node through `hub`, in the PR of the first
  connection with a security policy other than `None`. No connection that checks a
  certificate runs before. Its drop runs the queued delayed callbacks in at most 64
  passes, then aborts: a callback that queues itself at each pass is a defect. The
  hidden module `bench`, behind the feature `sim`, gives the benchmark and the
  allocation test a client on the loop. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6067067211,
  2026-10-08 19:05 UTC); the abort supersedes "panics" in that comment
  (https://github.com/synnaxlabs/foundation/pull/1982#issuecomment-6068349707,
  2026-10-08 20:22 UTC).
  The logger of the loop writes each message of level warning and up to fd 2, and
  drops the lower levels: it formats the line into a stack buffer of 512 bytes with
  `mp_vsnprintf` and sends it in one `write`, so a line allocates nothing, takes no
  `stdio` lock, and does not mix with a line of another thread. A longer line is cut,
  not dropped. Trigger: when `node` has a log, the loop takes its sink from its
  caller. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1982#issuecomment-6068103327,
  2026-10-08 20:07 UTC;
  https://github.com/synnaxlabs/foundation/pull/1982#issuecomment-6068349707,
  2026-10-08 20:22 UTC).
  Two timers due at one time run in an order that no code may depend on. The timer
  tree of the copy orders by due time, then by `id`, so that order is the order of
  their adds and a simulation with such timers replays; open62541 ranks them by heap
  address. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1982#issuecomment-6073878926,
  2026-10-09 03:49 UTC). The batch search of the copy goes in the order of the tree,
  so whether a current-time timer batches does not depend on addresses (approved by
  `laptop.architect-2`, 2026-10-09T05:27:21Z,
  https://github.com/synnaxlabs/foundation/pull/2106#issuecomment-6074900634).
  The TCP connection manager of `connector-opcua` is `connection::Manager`. It owns its
  loop and its connections, on one thread, and takes the clock, network, and randomness
  of `env` at `new`. `drive(run)` moves each connection on and polls `run` with the
  context of the drive until `run` gives a value, so `run` can poll its own sources. One
  drive of a manager runs at a time. A connect, a send, or a close from `run` or from
  another task on that thread wakes the drive. After each `run`, also the one that gives
  the value, the drive moves on each connection again after each connect, send, or close
  on it that the `run` or such a step asks for, so it can also read and call open62541
  back after that `run`. At most 256 sends wait on one connection: a send past them
  closes the connection. open62541 allocates each send at most at the send buffer size
  of its channel, so this bounds the memory of a connection at 256 send buffers. Sends
  wait from one pass to the next, and longer while a stream is full, so an owner keeps
  the chunks of its messages in flight at 256 or fewer. A client does so with its
  requests in flight times the chunks of a message. A close reads and drops what the
  peer sends while it writes what waits, closes its side, then reads until the peer
  closes its side, so that the drop sends no reset. It drops the stream with a warning
  10 s after the first close, so that a peer that reads slowly or never closes cannot
  hold it. Each connect, read, write, or close error gives a warning through the logger
  of the loop. The first close, or an error before it, gives `CLOSING` once, at the next
  run of the loop. The wake of a send on the thread of the drive: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6074418284,
  2026-10-09 04:43 UTC). The rest, before the send bound and the context of `run`:
  approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6085824086,
  2026-10-09 17:22 UTC). The send bound and the context of `run`: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086043036,
  2026-10-09 17:36 UTC). The sends between two passes, the passes before the value, and
  the order of a close: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086204518,
  2026-10-09 17:46 UTC). The moves of the woken connections alone before the value:
  approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086285343,
  2026-10-09 17:51 UTC). The moves after each `run`, and the `CLOSING` of the first
  close or an error before it: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086768731,
  2026-10-09 18:21 UTC).
  A server listens on the listener that its owner gives the manager at
  `Manager::listening`. Each accepted stream is a new connection that gets
  `ESTABLISHED`, with the context of the listen connection at the accept. As the POSIX
  manager does, the first `ESTABLISHED` of the listen gives `listen-address`, the host
  of its `address` param, and `listen-port`, from which the server makes its discovery
  URL, and that of an accepted connection gives `remote-address`. A listen with no
  `address` gives the address of the listener as `listen-address`, where the POSIX
  manager gives the host name. A listen on each address with no `address` gives neither,
  so the server makes no discovery URL from it, since `env` has no host name. A listen
  takes one `address` at most, because the manager has one listener: an open with more,
  or with an `address` that is not a string, gives `BadInvalidArgument`. An accept error
  closes the listen connection with a warning, also an error of one stream after which
  the listener stays usable, because `env` gives both as `Error::Io`. This holds only
  while the one server is the test server: before a server serves users, the manager
  must keep listening after an error of one stream (#2005). Lost: the manager binds its
  own listener with `Net::listen` from the parameters. `address` is a host name and
  `Net::listen` takes a socket address, so the open would resolve in a hook that must
  give `ESTABLISHED` before it returns, and an owner that binds port 0 could not learn
  the port before it builds the URL of its server. Also lost: one constructor with an
  `Option<Listener>`, which a client gives as a literal `None` (`docs/claude/rust.md`;
  `laptop.director`,
  https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089322178,
  2026-10-09 21:10 UTC), and one with an enum argument, a new type that holds one
  value. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6088685286,
  2026-10-09 20:26 UTC). The two constructors: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089351505,
  2026-10-09 21:12 UTC), after the ruling of `laptop.director`. Supersedes the one
  constructor of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6088685286. The
  parameters of the first callbacks: approved by `laptop.architect-2` at `feb6c21a4`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089521550,
  2026-10-09 21:25 UTC).
  A server on the loop of a manager is `STOPPED` when its last connection closes after
  `UA_Server_run_shutdown`. That close can queue a delayed callback on the server, such
  as the removal of a session that is not activated, so its owner drives until the
  server is `STOPPED` and nothing is due (`event::Loop::due` is false).
  `UA_Server_delete` then frees the server and each session at once. Between that drive
  and the delete, the owner calls nothing that queues a delayed callback on the server,
  such as `UA_Server_addCertificates`: the next run of the loop would read the freed
  server. Approved by `laptop.architect-2` at `5ebf60ae2`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6091519998,
  2026-10-10 00:19 UTC).
