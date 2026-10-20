- **LINUX CI (2026-10-05, rewritten 2026-10-17)** Each PR runs Clippy and the tests of
  the workspace on Linux (x86-64), macOS (arm64), and Windows (x86-64), in the matrix
  of the job `check` in `ci.yaml`. Each of the required checks `check`, `macos`, and
  `windows`, of the job `os`, passes only when each part of `check` on each OS passed
  (CI PACE, #3418).
  Each PR also checks each feature of each crate alone on the three OSes, in the
  required checks `features`, `features-macos`, and `features-windows`: the matrix of
  the job `features`, which leaves out the crates that the entry of its OS in `check`
  leaves out. Linux on ARM runs in `arm.yaml`. Each job names its runner image
  (`ubuntu-24.04`, `macos-15`, `windows-2025`), never `-latest`, so that only a PR
  changes the OS release of a runner (GitHub still updates the tools of each image each
  week). Only Linux runs the format, layer, global, oracle, and dependency checks, loom,
  shuttle, Miri, the sanitizers, fuzzing, mutants, and the root tests.
  - Each OS lists what fails on it in its entry of the matrix of the job `check`, each
    item with the open issue that removes it: the crates that do not build with all
    their targets, the crates whose tests fail, and the tests that fail. An item is only
    a failure whose value on that OS no test can state yet: a cause that is not known,
    or a defect that its issue fixes. A test whose value on the OS is known asserts it
    with a `cfg` on its expected value. The PR that closes an issue removes each item
    that names only that issue, and removes the issue from an item that names others.
    The lists only shrink, other than this: a new crate enters a list, a new gate
    enters the `gates` list of an OS, and an item gains an issue once the code of its
    crate needs another `os` item, each only with the open issues that build on that OS
    the `os` items that its code needs (`laptop.director`, 2026-10-18T20:47:36Z,
    https://github.com/synnaxlabs/foundation/pull/3260#issuecomment-6254960151). An item
    also gains the open issue of each other cause of its failure on that OS that it does
    not name yet, because CI does not run the item there (`laptop.director`,
    2026-10-18T23:37:35Z,
    https://github.com/synnaxlabs/foundation/pull/3260#issuecomment-6256584153). The
    review checks that each such issue builds such an item or states such a cause. A
    crate item that leaves a list, with one `tests` item for each of its tests that
    still fails, each with an open issue, is a shrink. The ONE NODE milestone does not
    close while a list holds a crate, a test, or a gate.
  - A test skips an OS by an attribute only in a case of `docs/claude/testing.md`, "No
    `#[ignore]`": the OS does not have the behavior, no test can fix its value there, or
    its test needs an item of one OS to cause the case. Its reason is `only <OS>`, or
    starts with `only <OS>:`, where `<OS>` names each OS that runs the test: Linux,
    macOS, or Windows, two of them joined by `and` in that order, or Unix for Linux and
    macOS. A skip that keys on several OSes, of which only some have the behavior, is
    one `cfg_attr` for each OS, each with its own reason. Each other skip of an OS, a
    `needs <tool>` skip too, is an item of the list of that OS with its issue. The
    review checks that the reason is true of the behavior, not only of the tool. `cargo
    xtask skips` reads the module tree of each target of the workspace, other than the
    `ignore` attributes in the body of a macro call, in the job `check` on Linux. For
    each test, it finds the OSes of the lists on which its `cfg` and those of its
    modules build it and an `ignore` skips it on the runner of that OS in the job
    `check`, and checks each reason of those `ignore` attributes. A test that those
    attributes skip on each OS on which it builds has no OS to name, so each of them is
    refused, unless a plain `#[ignore]` skips the test on each OS. It also refuses a
    `path` in a `cfg_attr`.
  - A `cfg` that names a target and leaves out of an OS a test that no item of the same
    path builds there is a gate, and so is a `cfg_attr` that applies `test` on only some
    OSes. A gate is an item of the `gates` list of each OS that it leaves out, as
    `- <file> <path in the file> # #<issue>`, or, in a case of "No `#[ignore]`", the
    first line of the doc of the item is `Only <OS>` or `Only <arch>`, or starts with
    either and `:`. `<OS>` names each OS on which the item builds, as for a reason.
    `<arch>` is the `target_arch` value (`aarch64` or `x86_64`) of each runner on which
    the item builds, that of `arm.yaml` too, and of no other. The list comes first, so a
    doc covers only each OS that no list item names, and the review checks its claim on
    each of those OSes. A gate covers a test module, a test function, a test in the body
    of a macro call or in a brace group of that body, and each function of a test or
    bench target with no harness. A branch of `cfg_if!` builds where its `cfg` holds and
    no `cfg` before it in its chain does. `cargo xtask skips` refuses each other gate,
    and each list item that names no gate. A `cfg` on a statement, and a test that a
    macro makes from tokens outside its call, are not read.
  - For `/review`, CI does not run an OS for a crate or a test on the list of that OS
    (`laptop.director`,
    https://github.com/synnaxlabs/foundation/pull/2960#issuecomment-6229528745).
  - OS-specific code goes only in `os`. Each `cfg` that leaves an OS out of a module or
    a public item of `os` names the issue that removes it, unless it picks the code of
    each OS (as `allocate` and `net::unsent` do) or the item is a test seam behind
    `sim` that only the tests of `os` call (as `memory::available_under` reads the
    cgroups of Linux). A test of a feature that only one OS has follows
    `docs/claude/testing.md`.

  History and sources:
  - The person, 2026-10-05 (#574), so CI ran only on Linux: "As long as our systems
    are designed to cross compile i'm ok wiht only testing against linux for an alpha.
    as long as the system is designed for cross os deployment".
  - The person, 2026-10-16, relayed word for word by `laptop.monitor` at
    2026-10-16T23:14:36Z
    (https://github.com/synnaxlabs/foundation/issues/2954#issuecomment-6226448224):
    "not cross compilation but full 3 platform support".
  - `laptop.monitor`, the body of #2954 at 2026-10-17T00:25:02Z: the pinned images,
    the lists with their issues, "The list only shrinks", the ONE NODE condition, and
    loom, shuttle, Miri, the sanitizers, and mutants on Linux only.
  - `laptop.director`, 2026-10-17T03:54:33Z, the form of the lists and what an item
    may be
    (https://github.com/synnaxlabs/foundation/pull/2960#issuecomment-6229309642).
  - `laptop.architect-2`
    (https://github.com/synnaxlabs/foundation/issues/2954#issuecomment-6226439164):
    `os` has no exception, and each `cfg` that leaves an OS out of a module of `os`
    names its issue.
  - `laptop.architect-2`, the public items of `os` and the two exceptions of the `cfg`
    rule (https://github.com/synnaxlabs/foundation/pull/2960#issuecomment-6229653306),
    and the narrowed second exception
    (https://github.com/synnaxlabs/foundation/pull/2960#issuecomment-6229892713).
  - Fuzzing and the root tests on Linux only, lists of tests, the review rule above,
    and the ONE NODE condition on tests: `box1.builder-1` in #2960, sent to
    `laptop.monitor` as key choices, and accepted by `laptop.monitor` at
    2026-10-17T03:43Z
    (https://github.com/synnaxlabs/foundation/pull/2960#issuecomment-6229189616).
  - The format, layer, global, oracle, and dependency checks on Linux only:
    `box1.builder-1` in #2960, accepted by `laptop.monitor`
    (https://github.com/synnaxlabs/foundation/pull/2960#issuecomment-6229520072): they
    read the source and the metadata, which are the same on each OS. On the condition of
    that acceptance, the dependency check covers the macOS and Windows targets:
    `deny.toml` sets no `targets`, so `cargo deny` checks the dependencies of each OS.
  - The feature check on each OS: `laptop.monitor`, 2026-10-17T05:02:02Z, which
    withdrew its acceptance of that check on Linux only
    (https://github.com/synnaxlabs/foundation/issues/2995#issuecomment-6229925039): "A
    feature set that does not build on Windows, and that Linux CI passes, breaks that
    rule." Each OS is its own job, beside the test jobs, so it adds no wall time while
    it is not the slowest job of the run. The times of the three jobs go on #2954, and
    a feature job that becomes the slowest job of the run goes back to `laptop.monitor`
    with its time.
  - The skips by an attribute: `laptop.director`, 2026-10-17T07:39:56Z, option 2
    (https://github.com/synnaxlabs/foundation/issues/3015#issuecomment-6231310442), on
    the gap that `laptop.monitor` found at 2026-10-17T07:32:46Z
    (https://github.com/synnaxlabs/foundation/issues/2954#issuecomment-6231250077).
  - The gates on the lists: `laptop.director`, 2026-10-17T16:02:01Z, option 1
    (https://github.com/synnaxlabs/foundation/issues/3015#issuecomment-6236264009).
    The doc form `Only <OS>`, which covers only each OS that no item names, and whose
    claim the review checks on each of those OSes: `laptop.director`,
    2026-10-18T04:21:59Z
    (https://github.com/synnaxlabs/foundation/issues/3015#issuecomment-6245603833).
    The doc form `Only <arch>`: `laptop.director`, 2026-10-18T05:10:43Z
    (https://github.com/synnaxlabs/foundation/pull/3134#issuecomment-6245970187), and
    its reading with the runner of `arm.yaml`, 2026-10-18T14:02:34Z
    (https://github.com/synnaxlabs/foundation/pull/3134#issuecomment-6250818913).
  - A new gate only with the issues of the `os` items that it needs: the person left the
    choice to `laptop.monitor`, 2026-10-18T14:45Z: "Your choice as to what is correctly
    going to balance factory pace with quality software dev". `laptop.monitor` chose
    option (b), as `laptop.director` recommends
    (https://github.com/synnaxlabs/foundation/pull/3134#issuecomment-6251149697).
  - A new crate on a list: the person, relayed by `laptop.monitor` at
    2026-10-17T17:30:12Z
    (https://github.com/synnaxlabs/foundation/pull/3048#issuecomment-6237654845):
    "Yeah fine". So `sdk` entered the Windows lists with #682 and #2955.
  - An item that gains an issue once the code of its crate needs another `os` item:
    `laptop.director`, 2026-10-18T20:47:36Z
    (https://github.com/synnaxlabs/foundation/pull/3260#issuecomment-6254960151). So the
    `sdk` items gain #2956 (#3260), for `os::stored`, which `sdk::Node::local` needs.
  - A crate item that becomes `tests` items: `laptop.monitor`, 2026-10-18T09:15:35Z,
    in place of `laptop.director`, which wrote the PR
    (https://github.com/synnaxlabs/foundation/pull/3163#issuecomment-6248122830): "a
    crate item that leaves a list, replaced by one `tests` item for each of its tests
    that still fails, each with an open issue, is a shrink." So `xtask` leaves the
    Windows lists with 32 `tests` items of #3165 and #3166.
