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
  The C library and the POSIX headers of the plugins are a closed list of system
  headers (`SYSTEM_HEADERS`) that the copy may include, each found in a system
  directory as `cc` finds it. The list holds no clock header (`time.h`,
  `sys/time.h`): a PR that adds one needs the OK of the `connector` architect. The
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
  2026-10-08 13:31 UTC). The `UA_ARCH_HEADER` of the allocator below is the one flag
  of `build.rs` outside `flags.txt`. A `-W` flag with no `,` is a warning, which
  changes no code, so `collect` leaves it out by that pattern, not by name. Decided by
  `laptop.architect-2`
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
  without `alloc.h`. A test reads the archives that `build.rs` makes with it, and
  fails on each symbol outside the copy and `shim.c` that its closed list does not
  hold. The list holds no clock function and no libc function that gives or takes a
  heap pointer. Lost: `UA_ENABLE_MALLOC_SINGLETON` (a global), `--wrap=malloc`
  (`std::alloc` calls `malloc`, so it recurses), and 4 `-D` flags (a define gives no
  prototype, and C99 needs one). Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6067206932,
  2026-10-08 19:13 UTC; the header:
  https://github.com/synnaxlabs/foundation/pull/1981#issuecomment-6067771921,
  2026-10-08 19:46 UTC). The exception for `UA_ARCH_HEADER` in the flags passage
  above, the copy check without `alloc.h`, and the test of the archives: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1981#issuecomment-6068060133,
  2026-10-08 20:04 UTC).
