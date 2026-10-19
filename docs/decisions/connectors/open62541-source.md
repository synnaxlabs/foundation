- **OPEN62541 SOURCE (#435)** We copy the upstream source files of open62541, not the
  amalgamation: the amalgamation adds the POSIX clock and event loop even with
  `UA_ARCHITECTURE=none`. The 3 global clock functions give a fixed time. That is
  acceptable only with a closed list of the (file, enclosing function) pairs that may
  call one; a list per file would pass a new call in a listed file. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050922367,
  2026-10-08 02:27 UTC). A test of `connector-opcua` names the function of each clock
  key of `FUNCTION_SYMBOLS` and fails when its reason becomes false. For example,
  the copy compiles no ECC policy, which `encryptUserIdentityTokenEcc` needs (#2934,
  below). Approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2246#issuecomment-6094588326,
  2026-10-10 06:19 UTC) and `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2246#issuecomment-6099193878,
  2026-10-10 15:39 UTC). The sentence "A test of `connector-opcua` names the function of
  each clock key of `FUNCTION_SYMBOLS` and fails when its reason becomes false.":
  approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6102856606,
  2026-10-10 22:28 UTC) and `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6103121782,
  2026-10-10 23:02 UTC). The copy goes in `patches/open62541/`, and the `build.rs` of
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
  The check reads each undefined symbol of an object that a relocation in that object
  names (`nm -u` and `objdump -r`) and fails on each symbol outside the copy that is not
  on a closed list, with the file and the symbol. `SYMBOLS` admits a symbol for any
  file. Each other reference needs its key in `FUNCTION_SYMBOLS`. Each entry has its
  reason. A symbol goes in `SYMBOLS` only when it reads no clock, file, network,
  randomness, or process state. A symbol is outside the copy when no object exports it:
  a `static` function of one file does not hide a call of the OS function of its name
  from another. A key of `FUNCTION_SYMBOLS` with no reference fails, so a file that the
  build leaves out loses its keys. A new outside symbol also goes in `OUTSIDE` in
  `connector-opcua`, or in `PLUGINS` when only one file of the copy may name it, which
  list the outside symbols of the production build. A test fails on a name of `PLUGINS`
  in an object of another file, and when the object of its file does not name it. A name
  that the compiler makes from another call, so that an object names it only at some
  levels (`fwrite` from `fputs`), goes in `OUTSIDE`. This sentence: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3037#issuecomment-6237085184,
  2026-10-17 16:55 UTC). The words "or in `PLUGINS` when only one file of the copy may
  name it": approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3013#issuecomment-6231994142,
  2026-10-17 08:55 UTC). The sentence on the objects of `PLUGINS`: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3037#issuecomment-6236322678,
  2026-10-17 16:06 UTC). A header list is not a check: a listed header can include
  another (`pthread.h` includes `time.h`). So the check
  refuses no system header, and each header that the copy includes must be in the copy
  or in a system directory as `cc` finds it. Lost: a header list, and a deny list of OS
  symbols, which passes a call that it does not name.
  Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/1884#issuecomment-6060989375,
  2026-10-08 13:31 UTC). Supersedes the closed list of system headers of
  https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058446715. The
  sentences "`SYMBOLS` admits a symbol for any file. Each other reference needs its key
  in `FUNCTION_SYMBOLS`." and "A key of `FUNCTION_SYMBOLS` with no reference fails, so a
  file that the build leaves out loses its keys.": changed by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6099166264,
  2026-10-10 15:36 UTC); approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6099491752,
  2026-10-10 16:07 UTC). `SYMBOLS` holds memory and string functions, the 5 constructors
  that `shim.c` defines to abort, and one exception that reads process state: the
  allocator, whose addresses the OS places at random, so no result of the copy may
  depend on an address. On x86-64 the assembler makes `_GLOBAL_OFFSET_TABLE_` undefined
  in each object that reads the table, with no relocation, so only a reference that the
  C makes counts. The check builds with no stack protector, so the compiler adds no
  reference to its random canary, and a reference that the C makes fails. Approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6099166264,
  2026-10-10 15:36 UTC). A reference is an undefined symbol that a relocation names, and
  it is outside the copy when no object exports it: approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6093780083,
  2026-10-10 04:30 UTC). When `SYMBOLS` does not admit a reference in a function, in any
  file, and it is not the address of a clock, the check keys it by (file, function,
  symbol, access), read from `objdump -dr` as the clock check reads a call. The function
  is the whole label of the function symbol, such as `parse.0` for a nested function,
  and the access is a call or an address. So a listed symbol in a new function of a file
  fails, and so does the address of a listed symbol that its function only calls. A
  function that has the address of a listed name is that listed function: a weak alias
  gives one function two names. An empty listed function before a new one gives the new
  one its address only with no `-ffunction-sections`. That is an edit made to hide, and
  review of each copy PR covers it. The function at the address of a listed name:
  decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6106749302,
  2026-10-11 07:37 UTC); approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6107095861,
  2026-10-11 08:23 UTC). `FUNCTION_SYMBOLS` lists these keys, the clock calls among
  them. A reference from outside a function fails when `SYMBOLS` does not admit it.
  Decided by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6094336509,
  2026-10-10 05:46 UTC). The whole label and the access: approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6094746609,
  2026-10-10 06:40 UTC). It refines the (file, enclosing function) list of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050922367 by symbol
  and access. A key may name a symbol that is not a clock, with its reason. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6099166264,
  2026-10-10 15:36 UTC). Supersedes the (file, symbol) pairs of
  https://github.com/synnaxlabs/foundation/issues/1884#issuecomment-6060989375. The text
  from "When `SYMBOLS` does not admit" to here, less the three sentences from "A
  function that has the address of a listed name" that `laptop.architect-2` decided on
  2026-10-11: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6099166264,
  2026-10-10 15:36 UTC); approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6099229278,
  2026-10-10 15:42 UTC). The check fails on every reference to a clock function that is
  not a call, also one in code. A call relocation counts as a call only in a section
  that `objdump -d` disassembles. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058446715,
  2026-10-08 11:03 UTC). Supersedes the clock address rule of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6057554572. The
  check stands against a clock reference that the compiler makes from C in the copy,
  from a new tag or from our patch. It does not stand against an edit made to hide
  from it, such as assembly that stores a function's address: review of each copy PR
  covers that. The check reads each symbol name from the text of `nm` and `objdump`, and
  `objdump` does not show each name exactly: `g+0x1` is a name, or `g` with an addend.
  So the check fails on each symbol whose name holds a character other than an ASCII
  letter, a digit, `_`, `.`, or `$`. A name that holds a newline reads as two names in
  `nm`, so this rule does not see it. No C identifier gives such a name: it is an edit
  made to hide, and review of each copy PR covers it. Lost: a second tool and a second
  parser (`readelf -sW`) for that one name. The symbol name rule: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6105970028,
  2026-10-11 05:48 UTC). The copy compiles with `-fno-dollars-in-identifiers`, a line of
  `flags.txt`, so the connector and the check read `$` in the same way and no C
  identifier gives a function a name that holds `$`: on aarch64, `objdump` gives no
  function header to a function named `$x` or `$d`, as it gives none to a mapping
  symbol, and in a section that holds another function, the code of such a function
  reads as code of the function before it. `cc` reads a macro name only up to its `$`,
  in the two builds alike. The symbol name rule still admits `$`, because each mapping
  symbol holds one. Lost: the extent of each function from the symbol table, a second
  parser for a name that no file of the copy has. Lost: the flag in the check alone,
  with which the two builds read `#define X$ 1` in two ways. `cargo xtask open62541`
  writes that line after the flags of the release, and the check fails on a `flags.txt`
  without it. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6106749302,
  2026-10-11 07:37 UTC). The words "read `$` in the same way" and "no C identifier gives
  a function": stated by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6107095861,
  2026-10-11 08:23 UTC). A `#line` directive or a line marker in a copy file fails the
  check, because it moves the file that the include check reads. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1848#issuecomment-6058103514,
  2026-10-08 10:42 UTC). A builtin or an intrinsic that reads a clock or randomness,
  such as `__rdtsc()`, makes no symbol and no relocation, and a new tag can add it as
  honest C. So the check reads each instruction of `objdump -d` and fails on each
  mnemonic outside a closed list for the architecture of the build, with the file, the
  function, and the mnemonic. A move of a system register (`mrs`, `msr`) is keyed by its
  register too. The key of an x86 instruction keeps each prefix that `objdump -d` shows
  as a word before the mnemonic (`rep stos`, `notrack jmp`), so such a prefix has its
  own entry. A prefix that `objdump` shows in an operand, such as the segment of
  `fs:0x0`, is not in the key. The prefix rule: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6105970028,
  2026-10-11 05:48 UTC). Which prefix the key keeps: stated by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6106559609,
  2026-10-11 07:12 UTC). An entry goes in the list only when its instruction reads
  registers, flags, and memory, and no other state. A system register that the processor
  or the OS fills, such as a counter (`cntvct_el0`) or a random number (`rndr`), is
  other state. One exception is the thread pointer, which the thread-local state of the
  copy needs, and which is an address, as those of the allocator are: `mrs tpidr_el0` on
  aarch64, and on x86-64 a memory operand through `fs`, which the key does not keep. The
  check does not see what the memory that an instruction reads holds. So a read at a
  fixed place from the thread pointer passes on the two architectures. On x86-64 with
  glibc, the stack guard is at such a place (`fs:0x28`). From C that names no address, a
  compiler makes such a read only for a thread-local variable and, on x86-64 with glibc,
  for the stack protector, which the check builds without. A read of another place from
  the thread pointer in the C of the copy is an edit made to hide, and review of each
  copy PR covers it. Lost: a key that keeps the segment (`mov fs:`). The thread-local
  state of the copy needs its entry, which then admits each place from the thread
  pointer, as `mrs tpidr_el0` does. Lost: a key by the place (`fs:0x28`), a deny list of
  the places of one libc. The entry rule and the thread pointer: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6107930295,
  2026-10-11 10:10 UTC, for the sentences on a fixed place,
  https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6108089522, 2026-10-11
  10:30 UTC, and, for the sentence on a system register,
  https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6108643437, 2026-10-11
  11:41 UTC). Supersedes the entry rule ("only when it reads no clock, randomness, file,
  network, or process state") of
  https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6103788499. Lost: a
  deny list of the instructions that read a clock or randomness, which passes one that
  it does not name, such as one of a later extension. The closed list, the key of `mrs`
  and `msr`, and this "Lost": decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2232#issuecomment-6103788499,
  2026-10-11 00:32 UTC). No flag of the upstream build is lost with no error. Decided by
  `laptop.director`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6060613260,
  2026-10-08 13:10 UTC). `cargo xtask open62541` puts each flag of a compile in
  `flags.txt` (`-D`, `-I`, `-std`, and `CODE_FLAGS`, the flags that change the code) or
  in `LEFT_OUT`, a closed list with the reason of each, and fails on any other flag.
  `build.rs` and the check both read `flags.txt`, so the check reads objects compiled
  with each flag of `flags.txt`, as the objects of the connector are. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6060989849,
  2026-10-08 13:31 UTC). The words "each flag of `flags.txt`, as the objects of the
  connector are": approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6108089522,
  2026-10-11 10:30 UTC). The `UA_ARCH_HEADER` of the allocator below and the sanitizer
  flags below are the only flags that `build.rs` names outside `flags.txt`. The `cc`
  crate also gives each compile of the connector flags from the profile and the target
  of the Rust build, such as the optimization level, `-fPIC`, and `-g`. It adds the
  `CFLAGS` of the environment too. The check takes none of these flags from `build.rs`
  or from the `cc` crate. It adds its own `-g -O0` and `-fno-stack-protector` (above),
  and no other flag that changes the code. So the two builds differ in the macros that a
  flag of one build alone changes, such as `__OPTIMIZE__`, `__PIE__`, and
  `_FORTIFY_SOURCE`. Until #2381, the check does not read code that a file of the copy
  puts under such a macro. The two builds also differ in macros that no flag makes, such
  as `__FILE__` and `__clang__`. The check names each source `./` and its path in the
  copy, and `build.rs` names it by an absolute path. The check compiles only with GCC.
  `clang` compiles the connector when the `cc` crate picks it, and always with the
  address sanitizer or under `cfg(fuzzing)` (below). Until #2381, the check does not
  read code that only the connector build compiles for the value of such a macro. The
  two builds also differ with no macro. The check compiles at `-O0`, where
  `__builtin_constant_p` of a local that holds a constant is 0. A release build of the
  connector compiles at `-O3`, where it is 1. Until #2381, the check does not read code
  that only the connector build compiles from the same preprocessed text. These
  sentences: stated by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6107095861,
  2026-10-11 08:23 UTC, and
  https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6108238957, 2026-10-11
  10:49 UTC, for the flags of the `cc` crate and of the check, and
  https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6108737600, 2026-10-11
  11:53 UTC, for the difference with no macro, and
  https://github.com/synnaxlabs/foundation/pull/2336#issuecomment-6109104654, 2026-10-11
  12:38 UTC, for the macros that no flag makes and the words "that holds a constant"). A
  `-W` flag with no `,` is a warning, which changes no code, so `collect` leaves it out
  by that pattern, not by name. Decided by `laptop.architect-2`
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
  The feature `open62541` of `connector-opcua` compiles the copy and `src/shim.c` with
  `cc`. `shim.c` defines the 8 symbols that the copy leaves undefined: the 3 clock
  functions give 0, and the 5 POSIX constructors panic with their name through
  `connector_opcua_die` and abort, since our config always has an event loop. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6050922367,
  2026-10-08 02:27 UTC). The test server is in `src/server.c`, which only the feature
  `sim` compiles, and a test fails when the Rust outside tests or another `.c` or `.h`
  file of `src/` names an identifier that starts with `UA_Server`, or `shim_server_new`
  or `UA_random_seed`. So no production code reaches `setDefaultConfig`,
  `interruptServer`, or `UA_random_seed`. Lost: a smaller claim for that clock key,
  which no test then checks. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2309#issuecomment-6103245041,
  2026-10-10 23:18 UTC). The test stands against honest code. A name split by a line
  splice or a string continuation has no honest use, so review covers it. Lost: a scan
  on the tokens of rustc and of the C preprocessor. It adds a Rust parser and a
  preprocessor run to a test, for edits that review sees. Also lost: a fix of the
  scan for each case, because each of three fixes let in a new case. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2329#issuecomment-6104162271,
  2026-10-11 01:23 UTC). A test asserts the pragmas of `shim.c` and `server.c` after
  the preprocessor. It stands against honest code. A `#line` directive or a line marker
  in `shim.c` or `server.c` has no honest use, since both files are written by hand,
  so review covers it, and the `#line` rule of the copy check does not apply to them.
  If one is ever generated, that rule applies to it. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/1947#issuecomment-6067031461,
  2026-10-08 19:03 UTC; the line marker:
  https://github.com/synnaxlabs/foundation/pull/1947#issuecomment-6067179092,
  2026-10-08 19:12 UTC). The pragma test and the `#line` rule for `server.c`:
  approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2329#issuecomment-6103852696,
  2026-10-11 00:41 UTC).
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
  The copy, `shim.c`, and `server.c` allocate through the global allocator of the
  binary. `src/alloc.h` is the `UA_ARCH_HEADER` of both builds: it declares the 4
  functions of `src/alloc.rs` and defines `UA_malloc`, `UA_calloc`, `UA_realloc`, and
  `UA_free` as them, before `config.h` sets the libc calls as the defaults. They keep
  the C contract on `std::alloc`: a failure gives NULL and never panics, `malloc(0)`
  and `realloc(p, 0)` give a unique pointer that is not NULL, and each pointer is
  aligned to 16. No pointer crosses between the libc allocator and these: the objects of
  the copy, `shim.c`, and `server.c` call no libc function that gives or takes a heap
  pointer. Our change makes `UA_CertificateUtils_getThumbprint` of
  `plugins/crypto/openssl/certificategroup.c` give and take its block with `UA_malloc`
  and `UA_free`, in place of `malloc` and `free`. Our change: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3037#issuecomment-6236391062,
  2026-10-17 16:11 UTC). A crypto library that a later PR links
  keeps its own allocator. So the counting allocator of a test or benchmark binary
  counts C too. The copy check compiles without `alloc.h`. A test reads the archive that
  `build.rs` makes with it, and fails on each symbol outside the copy, `shim.c`, and
  `server.c` that its closed list does not hold. The list holds no clock function and no
  libc function that gives or takes a heap pointer. Lost: `UA_ENABLE_MALLOC_SINGLETON`
  (a global), `--wrap=malloc` (`std::alloc` calls `malloc`, so it recurses), and 4 `-D`
  flags (a define gives no prototype, and C99 needs one).
  Decided by `laptop.architect-2`
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
  The allocator and the archive test for `server.c`: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2329#issuecomment-6103852696,
  2026-10-11 00:41 UTC).
  A NULL from a constructor of `shim.c` stops the process through `alloc::exhausted`,
  which calls `std::alloc::handle_alloc_error` as the Rust allocator does on out of
  memory. Its message gives the size of a header alone, because a constructor does
  not give the size of the block that failed. A NULL inside the copy keeps the C
  contract. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2541#issuecomment-6140602800,
  2026-10-13 07:30 UTC).
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
  `event::Loop` owns, on one thread. Its monotonic time is the clock of `env`, and the
  UTC offset is 0. Its drop runs the queued delayed callbacks in at most 64 passes,
  then aborts: a callback that queues itself at each pass is a defect. The hidden
  module `bench`, behind the feature `sim`, gives the benchmark and the allocation test
  a client on the loop. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6067067211,
  2026-10-08 19:05 UTC); the abort supersedes "panics" in that comment
  (https://github.com/synnaxlabs/foundation/pull/1982#issuecomment-6068349707,
  2026-10-08 20:22 UTC).
  `dateTime_now` gives the wall time of the `event::Time` that `event::Loop::new`
  takes. Approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2574#issuecomment-6147646436,
  2026-10-13 14:18 UTC, and
  https://github.com/synnaxlabs/foundation/pull/2574#issuecomment-6147864860,
  2026-10-13 14:28 UTC). The kind gives the wall time of the node through
  `hub::writer::Writer::time`, the midpoint of mesh time. Decided by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/issues/1992#issuecomment-6147197858,
  2026-10-13 13:56 UTC). This meets condition 3 of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6067067211 for the
  loop of the kind. No connection that checks a certificate runs on
  `event::Time::monotonic`.
  `sim::serve` gives the wall time that its caller gives; the benches and the unit
  tests give the monotonic time from the Unix epoch (`event::Time::monotonic`).
  Approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2574#issuecomment-6147681882,
  2026-10-13 14:19 UTC). The server clock of `sim::serve` adds the span of
  `Variables::set_clock` (`laptop.architect-2`, 2026-10-19T12:02:54Z and 12:37:09Z:
  https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6266740401 and
  https://github.com/synnaxlabs/foundation/pull/3354#issuecomment-6267372432).
  `laptop.architect-2` moved the trigger of #1992 to "exit test 1 of #1737 passes"
  (https://github.com/synnaxlabs/foundation/issues/1992#issuecomment-6140198064,
  2026-10-13 07:03 UTC), and the person added this part to ONE NODE ("Yes thats
  fine"), relayed by `laptop.monitor`
  (https://github.com/synnaxlabs/foundation/issues/1992#issuecomment-6146887778,
  2026-10-13 13:40 UTC). The certificate part of #1992 keeps its trigger: the PR of
  the first connection with a security policy other than `None`.
  `bench` also gives a connection manager with the test server and its clients on the
  loop of the manager. Approved by `laptop.architect-2` at `3b508e494`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093126338,
  2026-10-10 03:05 UTC).
  `bench::client` gives a test server on its own manager, and clients of it that each
  have their own manager, as the kind uses them. Each new loop sets the start value of
  the generator of its thread again, from the next value of its `Rng`, and each loop on
  that thread then draws from that one generator. So the caller gives each loop on a
  thread an `Rng` whose next value differs: after the same start value, the generator
  gives the same numbers again. Approved by `laptop.architect-2` at `c1285785f`
  (https://github.com/synnaxlabs/foundation/pull/2428#issuecomment-6111882520,
  2026-10-11 17:47 UTC). Lost: a `Loop::new` that mixes the next value with a draw from
  the generator, so that equal values give different numbers. A run then depends on what
  its thread drew before, so the same value does not give the same run, and the first
  loop of a thread has no start value to draw from. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2428#issuecomment-6111912275,
  2026-10-11 17:51 UTC).
  `bench::client::Server::scope` closes its server also after a panic in its body,
  which it then resumes, as `bench::Manager::scope` does. Supersedes, for a panic in
  the body, the leak in the doc of `Server::scope` approved at `c1285785f`
  (https://github.com/synnaxlabs/foundation/pull/2428#issuecomment-6111882520). Lost: a
  scope that leaks the server and its manager after a panic. The two fixtures of the
  crate then have two forms after a panic, and each test of a panic through the scope
  runs in a child process with no leak check, so a leak of the client under that test is
  not found.
  Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2428#issuecomment-6130241056,
  2026-10-12 18:31 UTC).
  The logger of the loop gives each message of level warning and up to the log of the
  node, and drops the lower levels: when `connector_opcua_admit` in `src/log.rs` finds
  room under the bound of LOG OUTPUT (11), it formats the line into a stack buffer of
  512 bytes with `mp_vsnprintf` and gives it to `connector_opcua_put_line`, which writes
  one event with the span of its loop as its parent: the span of the run for the loop of
  a run (LOG OUTPUT (11)), and no span for a browse (#3108), the test server, and the
  loops of `bench::Client` and `bench::client::Client`; `bench::Log` takes the span that
  its caller gives. A longer line is cut, not dropped. The line of a refused call and
  the line before an abort of `shim.c` go to `connector_opcua_die`, which panics with
  it, so the panic hook writes it before the abort: an event in the queue of the log is
  lost at an abort.
  `shim.c` names no `write` and no Win32 function (#2612; `laptop.architect-2`,
  2026-10-18T02:16:27Z,
  https://github.com/synnaxlabs/foundation/issues/2606#issuecomment-6244633633).
  Supersedes the write to fd 2 of
  https://github.com/synnaxlabs/foundation/pull/1982#issuecomment-6068103327 (item 1)
  and the Windows path of
  https://github.com/synnaxlabs/foundation/pull/2594#issuecomment-6153806001.
  Decided by `laptop.architect-2`
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
  of `env` at `new`. `drive(check)` moves each connection on, runs the loop of
  open62541 with no wait, and then polls `check` with the context of the drive, until
  `check` gives a value. So `check` only reads the state that the run left, and can poll
  its own sources; no caller runs the loop
  (https://github.com/synnaxlabs/foundation/issues/2415). `bench::Manager::ask` asks
  for its read after the run of the loop. One drive of a manager runs at a time. A
  connect, a send, or a close from `check` or from another task on that thread wakes
  the drive. After each `check`, also the one that gives the value, the drive moves on
  each connection again after each connect, send, or close on it that the `check` or
  such a step asks for, so it can also read and call open62541 back after that `check`,
  except the listen connection after an accept, which moves on only after the next
  `check`. The run of the loop before each `check`: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6207817058,
  2026-10-16 04:04 UTC). It supersedes
  https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086043036 and
  https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086768731 for the
  order of the run. At most 256 sends wait on one connection: a send past them closes
  the connection. open62541 allocates each send at most at the send buffer size of its
  channel, so this bounds the memory of a connection at 256 send buffers. Sends wait
  from one pass to the next, and longer while a stream is full, so an owner keeps the
  chunks of its messages in flight at 256 or fewer. A client does so with its requests
  in flight times the chunks of a message. A close reads and drops what the
  peer sends while it writes what waits, closes its side, then reads until the peer
  closes its side, so that the drop sends no reset. It drops the stream with a warning
  10 s after the first close, so that a peer that reads slowly or never closes cannot
  hold it. A manager reads each stream into its one buffer of 64 KiB, since open62541
  copies what it keeps of a read. At most 100 streams close at once: a close past that
  drops the stream that started to close first, before its close ends, with a warning.
  So a server of the minimal config holds at most its 100 secure channels and 100
  closing streams (#2262). Approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2271#issuecomment-6099183891,
  2026-10-10 15:38 UTC, and
  https://github.com/synnaxlabs/foundation/pull/2271#issuecomment-6103268271,
  2026-10-10 23:21 UTC). Supersedes
  https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6085824086 for the
  memory of a manager, and
  https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086204518 for a
  close past the cap, which ends before its drain. Each connect, read, write, or close
  error gives a warning through the logger of the loop. The first close, or an error
  before it, gives `CLOSING` once, at the next run of the loop. The wake of a send on
  the thread of the drive: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6074418284,
  2026-10-09 04:43 UTC). The rest, before the send bound and the context of `check`:
  approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6085824086,
  2026-10-09 17:22 UTC). The send bound and the context of `check`: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086043036,
  2026-10-09 17:36 UTC). The sends between two passes, the passes before the value, and
  the order of a close: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086204518,
  2026-10-09 17:46 UTC). The moves of the woken connections alone before the value:
  approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086285343,
  2026-10-09 17:51 UTC). The moves after each `check`, and the `CLOSING` of the first
  close or an error before it: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086768731,
  2026-10-09 18:21 UTC). The listen connection after an accept, and one accept between
  two runs of the loop: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2258#issuecomment-6099177882,
  2026-10-10 15:37 UTC). Supersedes
  https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086285343 and
  https://github.com/synnaxlabs/foundation/pull/2159#issuecomment-6086768731 for the
  listen connection after an accept.
  A server listens on the listener that `Manager::listening` binds on the socket address
  that its owner gives, with the options of each stream of the manager. The server of
  `sim::serve` listens on the listener that the caller of `sim::serve` binds, with the
  options of that bind, which `Manager::accepting` takes (OPCUA SIM). Approved by
  `laptop.architect-2` (2026-10-13T02:19:56Z:
  https://github.com/synnaxlabs/foundation/pull/2522#issuecomment-6136784257). Each
  accepted stream is a new connection that gets `ESTABLISHED`, with the context of the
  listen connection at the accept. A drive accepts at most one stream between two runs
  of the loop, as the POSIX manager accepts one for each run
  (`arch/posix/eventloop_posix_tcp.c:378-391` of v1.5.9). open62541 needs that run: at
  the limit, `purgeFirstChannelWithoutSession`
  (`patches/open62541/src/server/ua_server_binary.c:586-598`) also picks a channel that
  is already `CLOSING`, on which `UA_SecureChannel_shutdown` does nothing, so each
  accept before the run that gives that `CLOSING` adds one channel past the limit. A
  burst of 150 accepts in one pass held 149 secure channels against the limit of 100 in
  the `sim` test of #2255. After an accept, the drive moves the listen connection on
  only after the next run, so an accept costs one move of the listen connection, not a
  pass of the table. Lost: a wake and a full pass after each accept, so a burst of n
  streams cost n passes; and the manager gives each queued `CLOSING` after an accept, so
  it would run work of the loop, and a `CLOSING` would come by two paths. As the POSIX
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
  must keep listening after an error of one stream (#2005). Also before a server serves
  users, a pass must move only the connections that are woken: a pass moves each
  connection, about 63 to 107 ns each by the load of the host on the bench of #435 6b3,
  on box2, an Intel Xeon Platinum 8488C. Deferred by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6092329880,
  2026-10-10 01:45 UTC), with these words approved at `3b508e494`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093126338,
  2026-10-10 03:05 UTC). Lost for the listener: the open of the listen binds it from its
  parameters. `address` is a host name and `Net::listen` takes a socket address, so the
  open would resolve in a hook that must give `ESTABLISHED` before it returns. Also
  lost, for a server other than that of `sim::serve`: the owner binds the listener and
  gives it to `Manager::listening`, so the stream options hold only when the owner binds
  with the options of the manager. An owner that binds port 0 reads its port from
  `Manager::port`, which gives the port of the listener until a listen open takes it,
  and `Server::start` makes the URL of its server from that port. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2372#issuecomment-6108905320,
  2026-10-11 12:14 UTC). Supersedes "No accessor for the bound address" of
  https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093207582. Also lost:
  one constructor with an `Option<Listener>`, which a client gives as a literal `None`
  (`docs/claude/rust.md`; `laptop.director`,
  https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089322178, 2026-10-09
  21:10 UTC), and one with an enum argument, a new type that holds one value. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6088685286,
  2026-10-09 20:26 UTC). The two constructors `new` and `listening`: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089351505,
  2026-10-09 21:12 UTC), after the ruling of `laptop.director`. Supersedes the one
  constructor of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6088685286. The bind
  in `Manager::listening`: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093207582,
  2026-10-10 03:15 UTC). Supersedes "the listener that its owner gives" of
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6088685286 and
  https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089351505. The third
  constructor, `Manager::accepting`, takes the listener that the caller of `sim::serve`
  binds, with the options of that bind. The streams of the test server need not have the
  options of the client, so the cost of a second copy of the options is accepted for
  `sim::serve`. Supersedes the bind in `Manager::listening` of
  https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093207582 for
  `sim::serve`: approved by `laptop.architect-2` (2026-10-13T02:23:24Z:
  https://github.com/synnaxlabs/foundation/pull/2522#issuecomment-6136822544). The
  parameters of the first callbacks: approved by `laptop.architect-2` at `feb6c21a4`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089521550,
  2026-10-09 21:25 UTC).
  A server on the loop of a manager is `STOPPED` when its last connection closes after
  `UA_Server_run_shutdown`. That close can queue a delayed callback on the server, such
  as the removal of a session that is not activated, so its owner drives the loop until
  no delayed callback waits, and panics if one still waits after 64 runs, as a loop free
  does, or if the server is then not `STOPPED`. `event::Loop::delayed` gives whether one
  waits. `UA_Server_delete` then frees the server and each session at once. Between that
  drive and the delete, the owner calls nothing that queues a delayed callback on the
  server, such as `UA_Server_addCertificates`: the next run of the loop would read the
  freed server. The drive: approved by `laptop.architect-2` at `5ebf60ae2`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6091519998,
  2026-10-10 00:19 UTC). The drive until no delayed callback waits: approved by
  `laptop.architect-2` at `c04ad0db3`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6099448516,
  2026-10-10 16:03 UTC). Supersedes the drive until nothing is due of
  https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6091519998. The bound
  of 64 runs: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6102906547,
  2026-10-10 22:34 UTC). The delete and the calls between: approved by
  `laptop.architect-2` at `1816b9c85`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6090016795,
  2026-10-09 22:04 UTC). The panic in place of a drive until `STOPPED`: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093544897,
  2026-10-10 04:00 UTC). Supersedes the drive until `STOPPED` of
  https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6091519998.
  The one function that calls `UA_Server_run_shutdown`, does that drive, and
  deletes the server, with no call between: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6099099141,
  2026-10-10 15:28 UTC), as `connection::Manager::close_server`. Supersedes the
  `delete_server` sentence approved at `3b508e494`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093126338). That
  function is `server::Server::close`, on a `server::Server` that owns the server:
  approved by `laptop.architect-2` as a later step of 6c on #435
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093207582,
  2026-10-10 03:15 UTC). It is behind `cfg(any(test, feature = "sim"))` until a
  server that serves users calls it: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6099099141,
  2026-10-10 15:28 UTC), which moves to that function the cfg on `delete_server`
  approved at `3b508e494`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6093126338,
  2026-10-10 03:05 UTC). `server::Server` holds the started test server.
  `Server::start` is an `unsafe fn` whose caller deletes the server before its manager
  drops, and it takes the port of the server from the listener of the manager.
  `Server::close` panics before the stop when its manager does not hold the loop of
  the server: approved by `laptop.architect-2` at `d6d834286`
  (https://github.com/synnaxlabs/foundation/pull/2372#issuecomment-6106770355,
  2026-10-11 07:40 UTC). The `host` of `Server::start`, from which and the port it makes
  the one server URL: approved by `laptop.architect-2` at `c9a61bcaf`
  (https://github.com/synnaxlabs/foundation/pull/2372#issuecomment-6107958993,
  2026-10-11 10:14 UTC). `host` is a `&CStr`, so `start` has no panic for a NUL: decided
  by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2372#issuecomment-6108905320,
  2026-10-11 12:14 UTC). Supersedes `host: &str` of that comment. The one function:
  asked by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2180#issuecomment-6089865399,
  2026-10-09 21:51 UTC). The scans of the Rust outside tests in `link` count only the
  code behind `test` or `feature = "sim"` as test code, and `open62541` stays a product
  cfg: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6099099141,
  2026-10-10 15:28 UTC). The test of the `ua_server.c` patch stops and deletes its
  server itself, because it checks `STOPPING` between the stop and the drive. That
  exception: approved by `laptop.architect-2` at `c04ad0db3`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6099448516,
  2026-10-10 16:03 UTC). The test of the panic at the bound deletes its server itself
  after that panic, because `Server::close` panics before its delete, and the delayed
  callbacks that still wait are those of the test, which do not read the server. That
  exception: approved by `laptop.architect-2` at `08c753cbe`
  (https://github.com/synnaxlabs/foundation/pull/2212#issuecomment-6103350388,
  2026-10-10 23:32 UTC). The test of a close through another manager stops and
  deletes its server itself, because `Server::close` panics before its stop and takes
  the server. That exception: approved by `laptop.architect-2` at `c9a61bcaf`
  (https://github.com/synnaxlabs/foundation/pull/2372#issuecomment-6107958993,
  2026-10-11 10:14 UTC).
  A `client::Client` owns its connection manager, and so its loop, and its `UA_Client`.
  Its config leaves `connectivityCheckInterval` at 0, as `Client::idle` checks the
  connection itself (OPCUA KIND): it reads the server state when 10 s pass from the
  start of the last read, so the gap between two requests while it waits is 10 s
  plus at most the request timeout. A check goes through `Client::read`, so it shares
  the bound of 9 requests in flight, and a check that a wait leaves when it ends stays
  in flight until its answer. Between two checks, `idle` reads the state of the client
  after each run of the loop, and ends at once when the channel is not open or the
  session is not active. Lost: the check of open62541, which drops each status
  but `BadTimeout`; a check on each housekeeping run at each rate, which reads at 10 Hz
  for nothing; and no check, which loses an idle session at its timeout. Decided by
  `laptop.architect-2` (2026-10-16T03:46:32Z,
  https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6207619278), and the
  end at once on a lost channel or session (2026-10-16T04:04:38Z,
  https://github.com/synnaxlabs/foundation/pull/2809#issuecomment-6207818990).
  Its `close` has a grace of 1 s on the `env` clock, a private const. It first waits
  for the answer to a CreateSession in flight. The server refuses a CloseSession sent
  before that answer, since it has no session token, and when the answer then comes,
  open62541 activates the session, which the server keeps after the channel closes.
  Then `close` sends a CloseSession with `UA_Client_disconnectAsync` and drives its
  manager until the channel is closed. When the grace ends first, it closes the channel
  with `UA_Client_disconnectSecureChannelAsync` and drives its manager until the
  channel is closed. So a close ends within the grace plus the time to close the
  channel, and the CreateSession wait is in the same grace. The close is not raced with
  a cancel. A client that lost its channel, or whose server does not answer in the
  grace, leaves its session on the server until it times out
  (https://github.com/synnaxlabs/foundation/issues/2400). Lost: the request timeout as
  the bound, which is a stop of up to 10 s for each connector on a server that stops
  answering. Lost: a close with no wait, which leaves a session on each live server
  for its session timeout (1 h), so restarts fill a server with few sessions. Lost: no
  wait after the CloseSession, as a server that reads the request after the channel
  closes may drop it. A server that answers the CreateSession after the grace can keep
  that session until it times out, as after a lost channel. The open62541 test server
  drops it, as the session is not active when the channel closes. Ruled by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6206438915,
  2026-10-16 02:08 UTC), with the CreateSession wait in the grace as its amendment
  (https://github.com/synnaxlabs/foundation/issues/2622#issuecomment-6206868666,
  2026-10-16 02:44 UTC). The wait works
  around `src/client/ua_client_connect.c` of open62541: `UA_Client_disconnectAsync`
  (lines 2956 to 2976) sets the state CLOSING and sends a CloseSession with no token,
  and the callback of the CreateSession (line 1734) then writes CREATED over CLOSING.
  Lost: a patch of the copy that defers the CloseSession until the CreateSession answer.
  It waits for the same answer, so the close ends no sooner, and it changes the session
  state machine of open62541, where LOCAL PATCHES asks for a hand mutant of each line
  that it changes. When a new release replaces the copy, check whether it still sends
  that CloseSession, and remove the wait when it does not. The drop of a
  `client::Client` calls `shim_client_free`, which stops the loop, then deletes the
  client. On a stopped loop, open62541 refuses the synchronous CloseSession of the
  delete and does not wait for the channel to close, so the delete sends no
  CloseSession, and the session stays on the server until it times out. Since the drop
  stops the whole loop, the loop of a client holds no other client or server. Before a
  run returns, the kind closes its client with `close`. A drop is only for a run whose
  future drops or that panics, because each session that a drop leaves holds one of the
  sessions of its server until then. A read keeps its answer in the client, which
  outlives the delete that calls the callback of each read in flight, and the callback
  drops the answer of a request that is not the read in flight. Lost: a drop that sends
  the CloseSession. A delete on a started loop sends it in a synchronous call, and that
  call waits for an answer that only a drive of the manager reads. Proposed in
  https://github.com/synnaxlabs/foundation/pull/2397. Approved by `laptop.architect-2`,
  with the rule for the kind and the lost design as its texts
  (https://github.com/synnaxlabs/foundation/pull/2397#issuecomment-6109221870,
  2026-10-11 12:52 UTC). The wait for the CreateSession, its cause and bound, its lost
  design, and the close of a client that lost its channel: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/2397#issuecomment-6110582857,
  2026-10-11 15:26 UTC).
  A client keeps at most 9 reads in flight (`READS`, 16 before #3097, which bounds the
  queue of a channel by it: OPCUA READ). Only reads whose futures dropped or that
  timed out make more than one, and a read past the 9 first waits until one of them
  ends. A read sends the timeout hint 0, so neither open62541 (2^32 - 1 ms, 49.7 days)
  nor the server ends it, and it stays in flight until the server answers it or the
  channel closes. An open62541 timeout ends a request on the client only, so the count
  would not have the requests that the server still holds. The read gives `BadTimeout`
  at the request timeout of the client, its wait included. When no read ends in that
  wait, it gives `BadTooManyOperations`: the server holds 9 requests, and the client
  sends no read until it answers one. The kind closes a client that gives it and
  connects again. The bound is a constant from the sends of a connection, not from the
  rate of reads, as only a server that holds requests fills it. A client sends and takes
  at most 8 chunks in a message, and the test server sends at most 8, so the reads in
  flight times the chunks of a message stay at 72, below half of the 256 sends of a
  connection. The other half is for the requests of open62541 itself. open62541 checks
  the chunks of a message that it sends against its own count, not against the count of
  the peer (`src/ua_securechannel.c`, line 392). Lost: a read whose future drops cancels
  its request, since open62541 has no local remove, and `UA_Client_cancelByRequestId`
  sends one more request, and the server still answers the read; a read past the bound
  that fails at once, which fails reads that the late answer of a dropped read lets go
  on; and a client that closes its channel when no read ends in the wait, which gives
  the caller `BadConnectionClosed` and hides the cause. Proposed in
  https://github.com/synnaxlabs/foundation/issues/435#issuecomment-6127422590. Approved
  by `laptop.architect-2` at `8ab776da8`
  (https://github.com/synnaxlabs/foundation/pull/2450#issuecomment-6128818932,
  2026-10-12 16:59 UTC).
  An abort chunk (Part 6, 6.7.3) is an answer: a local change of the copy ends its
  request with the Error of the abort, or with `BadDecodingError` when the abort holds
  no Bad code. The release drops the abort and never ends the request. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2505#issuecomment-6134670336,
  2026-10-12 23:35 UTC). "No Bad code" in place of "no Error", so that an abort always
  ends its request with a Bad code: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3093#issuecomment-6243883839,
  2026-10-18 00:47 UTC). Lost: a finite open62541 timeout for a read again, which also
  ends the requests that the server still holds, so the count leaves them out; a close
  of the channel on an abort, which fails each other read and the session for one
  answer; and a count in Rust or in the shim, where no seam sees chunks.
  An abort deletes only the queued chunks of its request id, where the release deletes
  those of each request. The copy assembles chunks by request id, so with the release
  an abort between the chunks of another answer closes the channel. Decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3093#issuecomment-6243716532,
  2026-10-18 00:27 UTC). Supersedes "Keep `deleteChunks` as it is" of
  https://github.com/synnaxlabs/foundation/issues/2505#issuecomment-6134670336.
  #2934 builds the copy with `UA_ENABLE_ENCRYPTION_OPENSSL` against the `libcrypto` of
  the one `aws-lc-sys` of `Cargo.lock`, not a second crypto library. Directed by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6225109838,
  2026-10-16 21:32 UTC) and approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6230838999,
  2026-10-17 06:46 UTC). The `build.rs` of `connector-opcua` takes the headers from
  `DEP_AWS_LC_<version>_INCLUDE`, the `include` of the `links` key of `aws-lc-sys`: the
  headers that its build script puts in its `OUT_DIR/include`. `cargo xtask open62541`
  builds no `aws-lc-sys`, so it checks the copy against the headers of its source,
  `generated-include` and then `aws-lc/include`. Both read the symbol prefix from the
  `BORINGSSL_PREFIX` line of `openssl/boringssl_prefix_symbols.h`. The check admits in
  any file each function of aws-lc in `AWS_LC`, which reads no clock, file, network,
  randomness, or process state, by its name after the prefix. `AWS_LC_KEYS` keys each
  other call of aws-lc by its file and function, with its reason: each call that draws
  (TLS RANDOMNESS), and `ASN1_TIME_to_tm`, which reads the clock only on a NULL time,
  which the copy never gives. The validity check of a certificate reads the event loop
  of its group (a local patch), so no key admits `X509_cmp_current_time`, and `cargo
  xtask open62541` fails on a call of it (`laptop.architect-2`, 2026-10-17 06:46 UTC,
  https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6230838999, item 1;
  the loop as an input of the constructor, 2026-10-18 05:55 UTC,
  https://github.com/synnaxlabs/foundation/pull/3144#issuecomment-6246301508).
  `AWS_LC_INPUTS` lists the functions of aws-lc that read the clock, so the error of a
  new call of one asks for its reason, and a test fails when no test of
  `connector-opcua` names the function of each of their keys. With no password callback,
  a PEM reader of aws-lc reads a password from the console: it opens `/dev/tty`, or
  prints a prompt on stderr and reads stdin, and sets signal handlers. So each PEM read
  of the copy gives a callback of `PASSWORDS`: `UA_OpenSSL_refusePassword`, which
  refuses each encrypted PEM, or `privateKeyPasswordCallback`, which gives the password
  of its caller and refuses one that does not fit in the buffer of aws-lc. Each refusal
  returns -1: aws-lc refuses the PEM only on a result below 0, and on 0 it decrypts with
  the empty password. `cargo xtask open62541` finds each function of aws-lc that takes a
  `pem_password_cb` in the declarations of the output of `cc -E`, and fails on each name
  of one in the copy other than in a call that gives a callback of `PASSWORDS` at its
  place (#3017, #3027). It reads the readers from the declarations of one `cc -E` pass
  of each header under `openssl/` of aws-lc together, with the flags of aws-lc and none
  of the copy, so no file or header of the copy changes the set, and a pass that fails
  fails the check. It checks the calls in a `cc -E` pass of each file with the flags of
  its build, so it reads the unit that compiles. It refuses each header that the unit
  enters, by any name or link, under `openssl/` of a system directory other than an
  include directory of aws-lc. It also refuses each file that the unit enters and that
  it cannot find by its name, such as a name that is not UTF-8, so no header passes
  unread. A header of the copy under `openssl/` passes, such as its own
  `openssl/securitypolicy_common.h`. The readers come from aws-lc alone, and the check
  reads the calls of the unit that compiles. When a header of the copy hides one of
  aws-lc, a call of a reader keeps the name of the reader, or it has no prefix, so
  `FUNCTION_SYMBOLS` must list it. For a header under `openssl/`, this supersedes "So
  the check refuses no system header" of
  https://github.com/synnaxlabs/foundation/issues/1884#issuecomment-6060989375. In
  aws-lc-sys 0.45.0, the readers are the functions of `openssl/pem.h` that take a
  `pem_password_cb`, and `SSL_CTX_set_default_passwd_cb`, `PEM_read_SSL_SESSION`, and
  `PEM_read_bio_SSL_SESSION` of `openssl/ssl.h`. These three are in neither `AWS_LC` nor
  `AWS_LC_KEYS`, so the check refuses each call of them. It compiles each file with
  `-Werror=implicit-function-declaration`, as GCC 14 does by default, and with
  `-ftrack-macro-expansion=0`, so a call with no declaration fails the build on each
  GCC, also of a name of aws-lc, which is a macro of a system header. `AWS_LC` holds the
  PEM readers, since this check is the one guard of their password. Supersedes "The PEM
  readers belong in `AWS_LC_KEYS`" of
  https://github.com/synnaxlabs/foundation/pull/3013#issuecomment-6231723849, whose
  reason was the read of the console. The callbacks that return -1, and the check that
  reads the functions from the declarations: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6237471608,
  2026-10-17 17:20 UTC). The `-Werror` and `AWS_LC` sentences, and the `cc -E` sentence:
  decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3057#issuecomment-6238536489,
  2026-10-17 18:26 UTC;
  https://github.com/synnaxlabs/foundation/pull/3057#issuecomment-6238944949, 2026-10-17
  18:51 UTC; https://github.com/synnaxlabs/foundation/pull/3057#issuecomment-6241016919,
  2026-10-17 21:03 UTC). The pass of each header under `openssl/`: OK of
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3057#issuecomment-6241360552,
  2026-10-17 21:27 UTC). The `openssl/` header sentences: approved by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3073#issuecomment-6242174637,
  2026-10-17 22:22 UTC). The `openssl/` rules of round 1: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3073#issuecomment-6244284643,
  2026-10-18 01:34 UTC). Supersedes the two `openssl/` header sentences of
  https://github.com/synnaxlabs/foundation/pull/3073#issuecomment-6242174637. The text
  of the rules of round 1: approved by `laptop.architect`
  (https://github.com/synnaxlabs/foundation/pull/3073#issuecomment-6244707524,
  2026-10-18 02:25 UTC) and `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3073#issuecomment-6244728627,
  2026-10-18 02:28 UTC), which also decided "by any name or link". The sentence on a
  file that the unit cannot find by its name: decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3073#issuecomment-6245010018,
  2026-10-18 03:05 UTC). `OMITTED`
  in `cargo xtask open62541` is the closed list of the release files that the library
  compiles and the copy leaves out, each with its reason: the file stores,
  `create_certificate.c`, and each policy file other than `Basic256Sha256`,
  `Aes128_Sha256_RsaOaep`, `Aes256_Sha256_RsaPss`, and their common code. So the copy
  compiles only the three current policies and `None`, and the production code adds no
  other. `shim.c` defines each symbol that the rest of the copy names from a left-out
  file to panic with its name and abort, and a test calls each one. A test fails on
  another policy file in `sources.txt`. A test fails when the production code names a
  policy or a certificate group, or a function `UA_CertificateUtils_*`, other than the
  three policies in `POLICIES` of `shim.c`, `UA_CertificateGroup_Memorystore`,
  `UA_CertificateUtils_checkKeyPair`, `UA_CertificateUtils_getApplicationUri`, and the
  stubs of `shim.c` (`laptop.architect-2`, 2026-10-18 21:39 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6255470380, item 1).
  A test fails when the objects of `shim.c` or `server.c` name aws-lc or a symbol of
  `PLUGINS`. `UA_Openssl_Init` of `securitypolicy_common.c` keeps
  no state: the copy removes its `static` flag, which it wrote with no lock and which
  guarded only the calls for OpenSSL before 1.1, and the build fails when
  `OPENSSL_API_COMPAT` is defined. Lost: the about 35 file and clock calls of the
  left-out files, each keyed in `FUNCTION_SYMBOLS`; and a copy of each deprecated and
  ECC policy file. Decided by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6230888200,
  2026-10-17 06:51 UTC). The `UA_Openssl_Init` sentences: decided by
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3013#issuecomment-6231556855,
  2026-10-17 08:08 UTC). The `PLUGINS` sentence: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3013#issuecomment-6231723849,
  2026-10-17 08:26 UTC). The `AWS_LC_INPUTS` and `UA_Openssl_Init` sentences, and the
  PEM sentences before the `-Werror` sentence: approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3057#issuecomment-6238447456,
  2026-10-17 18:18 UTC).
  The split of #2934 PR 1c into 1c, the clock of the validity check with its tests at
  the certificate group, and 1d, the secure client and sim server, the test of each
  policy in each mode, and the replay test of TLS RANDOMNESS: OK of
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6246201281,
  2026-10-18 05:41 UTC). Both merge before PR 2, and 1d merges after 1c. The plan of 1d
  goes on #2934 before its code. The loop is an input of
  `UA_CertificateGroup_Memorystore`, which refuses NULL, not a member of
  `UA_CertificateGroup` that its owner sets: OK of `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3144#issuecomment-6246301508,
  2026-10-18 05:55 UTC). The test of `leaf.der` at `notAfter` is the test of item 1 of
  6230838999 that moves the clock past `notAfter`.
  The client part of 1d was in `server.c`, which only a `sim` build compiles; PR 2a of
  #2934 moves it into `shim.c`.
  1d meets the certificate part of #1992: each loop of a secure test connection runs on
  the wall time that its test gives, and a test moves each side past the `notAfter` of
  the certificate of the other side (the plan,
  https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6247720334).
  Approved by `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6247742342,
  2026-10-18 08:40 UTC). A server refuses an expired client certificate with
  `BadCertificateTimeInvalid`, the status that it sends, and 1d has no test of a switch
  of the certificate, which no client config reaches in 1.5.9: OK of
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6247943704,
  2026-10-18 08:56 UTC). The session group of the sim server stays the accept-all
  group, and the sim server sets no application URI: 1d opens only anonymous sessions,
  and no check that a test reaches refuses on the URI: at each start of a secure
  server, `verifyServerApplicationUri` logs one warning for each of the three
  policies, because the default URI of the minimal config does not match the URI of
  `server.der`. So a 1d
  connection checks two groups, not the three of item 1 of 6247742342: OK of
  `laptop.architect-2`
  (https://github.com/synnaxlabs/foundation/pull/3167#issuecomment-6248291820,
  2026-10-18 09:33 UTC). The first PR whose code reads an X509 user token, or has a
  client check the server URI, sets the session group or the URI in the sim server, with
  a test that fails without it.
  PR 2a of #2934 moves the client part into `shim.c`. The client sends the first URI of
  the subject alternative name of its certificate, from
  `UA_CertificateUtils_getApplicationUri`, a change to the copy ("Local patches" in
  `docs/dependencies.md`), so no second read of the name exists. Lost: our C on the
  `libcrypto` of aws-lc in `shim.c`, which only the OpenSSL plugins of the copy
  include; our own DER parser; and a Rust X.509 crate, a second parse of the
  certificate (`laptop.architect-2`, 2026-10-18 20:53 UTC,
  https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6255016583, as
  fixed in
  https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6255409915).
  Supersedes the policy index of the test helper and its abort in
  https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6247742342: the
  sim server offers an ordered list of policies, and each secure test connects
  through `Client::connect`. The client trusts one byte string: DER certificates back to
  back, when `d2i_X509` reads its first certificate, or else PEM certificates. The copy
  loads each certificate of each trust entry, and refuses an entry whose load stops
  before its end, and the constructor of the group gives that status (ruling A). PEM is
  read by the loop of `PEM_read_bio_X509_AUX` of aws-lc on a BIO of the full length,
  which takes `CERTIFICATE`, `X509 CERTIFICATE`, and `TRUSTED CERTIFICATE` blocks and
  skips the text outside them, not by the leaf loader of the copy, which drops a block
  that does not start with the exact line `-----BEGIN CERTIFICATE-----` (round 2 of
  review of #3271). This supersedes item 1 of
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6255744593. A DER
  entry with a byte after its last certificate, and DER then PEM in one entry, give
  `BadInternalError`. What aws-lc reads as text is dropped: a block with a damaged
  `BEGIN` line, DER bytes after PEM, and a damaged first DER certificate before a PEM
  block. So the connect fails closed with an untrusted certificate
  (`laptop.architect-2`, 2026-10-18 22:30 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6255959458; for the
  second, 2026-10-18 22:50 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256155598; and for
  the last, with the DER test, 2026-10-18 22:58 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256221476). A block
  that holds two certificates gives `BadInternalError`, since the AUX reader of aws-lc
  reads the bytes after the first certificate of a block of any label as trust
  settings (`laptop.architect-2`, 2026-10-18 23:35 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256562565, item 2,
  which supersedes that part of item 1 of 6256155598). A block with any byte after its
  certificate gives `BadInternalError`, also a `TRUSTED CERTIFICATE` with trust
  settings, since the verify of the group applies none. A `TRUSTED CERTIFICATE` with
  no settings reads. So the client never trusts a certificate past what its trust file
  says (`laptop.architect-2`, 2026-10-18 23:40 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256613286, which
  supersedes item 1 of 6256562565). Lost, with no form of them for now: a read of the
  settings for the use of the group, and the verify of aws-lc in place of the chain
  walk of the copy, which comes back only when a user needs a system trust bundle.
  Cost: a trust file with settings, such as a system bundle in `TRUSTED CERTIFICATE`
  form, is refused, and `openssl x509` writes it again without them. The person
  approved this form (`laptop.monitor`, 2026-10-18 23:41:44 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256621478): "Yeah
  fine".
  `laptop.monitor` accepted that the readers of aws-lc read the trust, and our own PEM
  parser lost (message 20c8b048 to `box1.builder-3`,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256452096). An
  issuer entry keeps the load of the release (`laptop.architect-2`, 2026-10-18 22:07
  UTC, https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6255744593,
  item 2).
  The validity check of the own certificate and the map of each refusal in `kind.rs`
  move to PR 2b (`laptop.architect-2`, 2026-10-18 21:37 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6255457069, items 1
  and 2). `laptop.monitor` accepted the trust as one byte string
  (2026-10-18 22:03 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6256452096), on one
  condition: the user never sees only `BadInternalError`. The check of PR 2b names the
  source and the certificate that does not parse, before the run, and the plan of PR
  2b names the test that pins that text
  (https://github.com/synnaxlabs/foundation/issues/2934#issuecomment-6255781688).
  `Client::connect` keeps its `Option`, on the condition that PR 2b makes `kind.rs`
  pass the value of the config, or splits `connect` or gives it an enum
  (`laptop.director`, 2026-10-18 21:48 UTC,
  https://github.com/synnaxlabs/foundation/pull/3271#issuecomment-6255562713).
