# R17: Ousterhout, A Philosophy of Software Design

Scope: the book (2nd edition, July 2021), the 2024-2025 discussion with Robert Martin,
the CS190 pages, the 2018 Google talk, and a 2025 interview. Tags: [V] verified in a web
source, [V1] verified in 1st-edition text only (2nd edition text not seen), [U]
unverified. Book quotes come from a public 1st-edition copy (English text beside a
Chinese translation) unless marked 2nd edition.

## 1. Principles and red flags

### Design principles (end-of-book summary)

The 15 items below are verbatim from the 1st-edition summary [V1]. No source reports a
change to items 1-15 in the 2nd edition.

1. Complexity is incremental: you have to sweat the small stuff. -> Complexity builds
   from many small additions, so each small one counts.
2. Working code isn't enough. -> Code that works but adds needless complexity is not
   done.
3. Make continual small investments to improve system design. -> Spend about 10-20% of
   time on design and cleanup, all the time, not in one big pass.
4. Modules should be deep. -> Much function behind a small interface.
5. Interfaces should be designed to make the most common usage as simple as possible.
   -> The common case needs the least knowledge from the caller.
6. It's more important for a module to have a simple interface than a simple
   implementation. -> The developer suffers so that the users do not.
7. General-purpose modules are deeper. -> Make the interface "somewhat
   general-purpose"; build only the functions you need now.
8. Separate general-purpose and special-purpose code. -> Push special cases up or down,
   out of the general mechanism.
9. Different layers should have different abstractions. -> Two adjacent layers with
   the same abstraction is a design smell.
10. Pull complexity downward. -> Handle unavoidable complexity inside the module, not
    through parameters or exceptions given to callers.
11. Define errors (and special cases) out of existence. -> Change the semantics so that
    the normal path covers the edge case.
12. Design it twice. -> Sketch radically different options before you commit.
13. Comments should describe things that are not obvious from the code. -> If the code
    already says it, the comment adds nothing.
14. Software should be designed for ease of reading, not ease of writing. -> The
    reader, not the writer, decides what is clear.
15. The increments of software development should be abstractions, not features. ->
    Design a whole abstraction at once; do not grow it one feature at a time.

2nd-edition addition: the new chapter "Decide What Matters" says "Separate what matters
from what doesn't matter ... Things that matter should be emphasized and made more
obvious; things that don't matter should be hidden" [V, 2nd-edition reader
highlight]. A 16th summary item with the wording "Separate what matters from what
doesn't matter and emphasize the things that matter" is from memory [U].

### Red flags (end-of-book summary)

The 14 names below match the 1st-edition summary [V1] and a list of 14 on dev.to [V].

- Shallow Module: the interface is not much simpler than the implementation.
- Information Leakage: one design decision shows up in more than one module.
- Temporal Decomposition: code structure follows execution order, not information
  hiding.
- Overexposure: to use a common feature, callers must know about rare ones.
- Pass-Through Method: a method only forwards its arguments to a method with a similar
  signature.
- Repetition: a nontrivial piece of code appears again and again.
- Special-General Mixture: special-purpose code is not cleanly apart from
  general-purpose code.
- Conjoined Methods: you cannot read one method without reading the other.
- Comment Repeats Code: all the comment says is plain from the code beside it.
- Implementation Documentation Contaminates Interface: an interface comment tells
  users implementation details they do not need.
- Vague Name: a name so imprecise that it says little.
- Hard to Pick Name: no precise, intuitive name fits, which hints at an unclean design.
- Hard to Describe: a complete comment for a method or variable must be long.
- Nonobvious Code: a quick read does not show what the code does or means.

### What the 2nd edition changed (author's site [V])

- New chapter "Decide What Matters" (chapter 21 per third-party notes [U]).
- Chapter 6, "General-Purpose Modules are Deeper", reworked and expanded, with material
  moved in from other chapters. Which sections moved: [U].
- New subsections in two chapters compare the book with Clean Code on method length
  and comments. Which two chapters: [U].
- The author offers the two new chapters plus the Clean Code sections as a free extract
  for 1st-edition owners.
- Naming: "Every word in a name should provide useful information; words that don't
  help to clarify the variable's meaning just add clutter" is in 2nd-edition reader
  highlights and not in the 1st-edition chapter 14 text, so it is likely new [V for the
  quote, U for "new"].
- In the Martin discussion he admits his TDD description was wrong and says he will fix
  it "in the next revision" [V]. A 3rd edition is not confirmed [U].

## 2. Complexity, and strategic versus tactical

Complexity [V1]: "anything related to the structure of a software system that makes it
hard to understand and modify the system."

- Symptoms: change amplification (a simple change needs edits in many places);
  cognitive load (how much a developer must know to do a task); unknown unknowns (you
  cannot tell which code to change or what you must know). Unknown unknowns are the
  worst.
- Causes: dependencies (code that cannot be understood or changed alone) and obscurity
  (important information is not obvious). Dependencies cause change amplification and
  cognitive load; obscurity causes unknown unknowns.
- More lines can be simpler if they cut cognitive load. Complexity grows in small
  pieces, so he asks for "zero tolerance".
- In the Martin discussion he restates it as information: how much a developer must
  hold in their head, and how accessible and obvious that information is [V].

Strategic versus tactical [V1]:

- Tactical: the main goal is to get a feature or fix working fast; each shortcut looks
  cheap and they add up. The "tactical tornado" ships fast and leaves a mess for others.
- Strategic: "Your primary goal must be to produce a great design, which also happens to
  work." Investments are proactive (try a couple of designs, write docs) and reactive
  (fix a design problem when you find it, do not patch around it). He asks for about
  10-20% of total time.
- CS190 notes: "Technical debt is another term for tactical programming" [V].

## 3. Positions

- **Comments.** Interface comments define the abstraction: what a caller must know,
  with side effects and exceptions. Implementation comments say what and why, not how
  [V1]. Every class, class variable, and method should get an interface comment;
  implementation comments are often unnecessary [V1]. "If users must read the code of
  a method in order to use it, then there is no abstraction" [V, reader highlight].
  Comments add precision (lower level) or intuition (higher level); a comment at the
  code's own level repeats it [V1]. Cross-module decisions need a findable home [V1].
  Comments belong in the code, not the commit log [V1, section title]. With Martin: he
  would write 5-10x more comment lines; missing comments cost "10-100x" more than wrong
  ones [V].
- **Comments first.** Write the interface comment before the body; this is a design
  tool. A long comment is a "canary in the coal mine": a sign of a bad abstraction
  (Hard to Describe). He argues comments first may be faster overall [V1]. CS190 lists
  "Writing comments before code" [V].
- **Method length and splitting.** "Length by itself is rarely a good reason for
  splitting up a method." Methods of hundreds of lines are fine with a simple signature
  and easy reading. Split only for cleaner abstractions: extract a subtask that each
  side can read alone, or split an interface that did unrelated things. Join methods
  that are shallow, duplicated, or conjoined. "Each method should do one thing and do it
  completely" [V1]. With Martin: the One Thing rule "lacks guardrails"; he keeps lock
  acquire and the critical section in one method [V].
- **TDD and unit tests.** Strong supporter of unit tests: they make refactoring safe
  (the Tcl byte-code rewrite had one bug after alpha) [V1]. Against TDD: it "focuses
  attention on getting specific features working, rather than finding the best design"
  [V1]. With Martin he prefers "bundling": write tens to hundreds of lines (a class or a
  few methods), then write full unit tests; code is not "working" until tested [V].
  Exception: when you fix a bug, write the failing test first [V1, and V in the 2025
  interview].
- **Exceptions.** "Exception handling is one of the worst sources of complexity." Cut
  the number of places that handle errors [V1]. Four techniques [V1]:
  1. Define errors out of existence: unset means "ensure gone", so no error; substring
     clamps the range.
  2. Mask: handle low in the stack (TCP resends lost packets).
  3. Aggregate: one handler high in the stack (one top-level web handler builds every
     error response).
  4. Just crash: print diagnostics and abort for rare errors you cannot handle well
     (out of memory, internal inconsistent data). This depends on the application: a
     replicated store must recover from I/O errors.
  Limit: "it is possible to take this idea too far". A team that masked all network
  errors made robust apps impossible. "When something is important, it must be
  exposed" [V1]. CS190: "use this idea judiciously" [V].
- **Design it twice.** Before each major decision, sketch radically different options,
  even ones you think are bad; do it for the interface and again for the
  implementation [V1].
- **Pull complexity downward.** "It is better for the developers to suffer than the
  users." Config parameters push complexity up; avoid them, compute defaults, and ask
  whether the user can really pick a better value [V1].
- **Different layer, different abstraction.** Each layer should change the abstraction
  [V1]. OK cases: dispatchers, and several implementations of one interface [V1].
  Decorators tend to be shallow; consider alternatives before writing one [V1].
- **Pass-through methods.** They add interface and dependency with no function. Fix by
  letting callers call the lower class, by moving responsibility, or by merging the
  classes [V1].
- **Pass-through variables.** Options: a shared object, a global (rejected: it blocks
  two instances in one process, which tests need), or his usual choice, a context object
  passed only to constructors and kept as a field. He admits contexts can become a
  grab-bag with global-like problems and should hold immutable values [V1].
- **General-purpose versus special-purpose.** "Somewhat general-purpose": the
  functionality should reflect your current needs, but the interface should not [V].
  General interfaces hide more information [V1].
- **Consistency.** It gives cognitive leverage. Document conventions, enforce them with
  tools, follow "When in Rome". "Having a 'better idea' is not a sufficient excuse to
  introduce inconsistencies." Do not force dissimilar things into one pattern [V1].
- **Code should be obvious.** A first guess about its behavior should be correct.
  "'Obvious' is in the mind of the reader": if a reviewer says it is not obvious, it is
  not. Event-driven code and generic containers make code less obvious [V1].
- **Performance.** Pick "naturally efficient" designs; know what is costly; measure
  (micro-benchmarks) before you change anything. "Simpler code tends to run faster";
  deep classes cross fewer layers. Last resort: design around the critical path.
  Imagine the minimum code for the common case in one method, then find a clean
  structure close to it, and take special cases off the critical path (ideally one
  `if`) [V1].
- **Software trends.** Getters and setters are shallow; design patterns are good when
  they fit, but people overuse them; agile and TDD risk tactical work [V1].
- **AI tools (2025 interview).** He expects AI to write more low-level code and design
  to matter more [V, as summarized by the interviewer]. The comparison of AI agents to
  "tactical tornadoes" is the newsletter's framing; that he said those exact words is
  [U].

## 4. Disagreements with Clean Code (Martin discussion, Sep 2024 to Feb 2025)

| Topic | Clean Code (Martin) | Ousterhout |
| --- | --- | --- |
| Method length | "Small, then smaller"; 2-4 lines; one-line `if` bodies | Length is rarely a reason to split; prefer deep methods |
| Split rule | "One Thing": extract when you can name it meaningfully | One Thing is vague and has no guardrails; it causes entanglement |
| Entanglement | Benefits outweigh it; method order helps | Conjoined methods are harder to read; merge them |
| Comments | "Always failures"; a net negative as practiced | Essential; 5-10x more lines; missing ones cost 10-100x more |
| Names vs comments | Long names replace comments | Short precise names plus comments |
| Internal interfaces | Little need to comment inside a team | Every interface needs a comment |
| Trust | Verify comments against code | Trust comments, read less code |
| TDD | Three laws, seconds-long cycles, red-green-refactor | "Bundling": design, code a unit, then test it |
| Unit of work | A test | An abstraction |
| Agreement | Unit tests essential; modular design good; over-decomposition is possible | Same |

Their joint `PrimeGenerator` rewrites showed the cost: Martin's split loops ran 3-4x
slower until he merged methods again [V]. Ousterhout's close: Clean Code fails "to focus
on what is important" and fails "to balance design tradeoffs" [V]. Martin says he put
some of Ousterhout's ideas and the whole document into Clean Code 2nd edition [V].

## 5. Sources

- https://github.com/johnousterhout/aposd-vs-clean-code (README, read in full)
- https://web.stanford.edu/~ouster/cgi-bin/book.php
- https://web.stanford.edu/~ouster/cgi-bin/aposd.php
- https://web.stanford.edu/class/cs190
- https://web.stanford.edu/~ouster/cs190-winter24/
- https://web.stanford.edu/~ouster/cs190-winter24/lectures/aposd
- https://www.youtube.com/watch?v=bmSAYlu0NcY (Talks at Google, 2018; confirmed through
  search results, not watched)
- https://talksatgoogle.libsyn.com/ep485-john-ousterhout-a-philosophy-of-software-design
- https://github.com/Cactus-proj/A-Philosophy-of-Software-Design-zh (1st-edition
  English text: docs/summary.md, ch02, ch03, ch04, ch07-ch11, ch13-ch15, ch17-ch20)
- https://dev.to/sportebois/software-design-red-flags-wisdom-nuggets-from-john-ousterhout-43i2
- https://www.goodreads.com/notes/58648004-a-philosophy-of-software-design/49553025-rocky?page=1
- https://www.goodreads.com/notes/58648004-a-philosophy-of-software-design/60311928-atthavit-wannasakwong/81c205f7-12d4-4258-be5b-659fde48d548
- https://newsletter.pragmaticengineer.com/p/the-philosophy-of-software-design (Apr 9,
  2025)
- https://lethain.com/notes-philosophy-software-design/
- https://www.binaryphile.com/2026/01/09/ousterhout-software-design-guide.html
- https://amstelden.com/philosophy-of-software-design-notes/
- https://dev.to/danlebrero/book-notes-a-philosophy-of-software-design-cna
- https://dev.to/markadel/a-philosophy-of-software-design-summary-pk9

## 6. Fit with the Foundation rules

- Deep modules: agrees; this is his main idea (chapter 4, principle 4).
- No pass-through functions: agrees (red flag). He extends it: dispatchers and several
  implementations of one trait are allowed, and decorators or wrappers are suspect.
- Pass-through at a layer boundary: partly conflicts. He has no boundary exception;
  he asks that the outer layer give a different abstraction, or that you merge layers.
- Inject dependencies, no mutable globals: agrees. He rejects globals because they stop
  two instances in one process. His context object fits only if passed to
  constructors, immutable, and kept small.
- Tests first: conflicts. He opposes TDD and prefers design, a unit of code, then full
  tests. He agrees with tests first for bug fixes (a regression test before the fix).
- Short comments: agrees on no restating code, no implementation in interface docs,
  and long comments as a design smell (Hard to Describe). Conflicts on volume: he wants
  an interface comment on every type, field, and method, and fears missing comments far
  more than wrong ones.
- Comments only for non-obvious behavior: agrees for implementation comments
  (principle 13); he does not apply it to interface comments.
- No defense in depth, let it crash: agrees with "just crash" for internal
  inconsistency and with one top-level handler (aggregation). It extends the rule: first
  try to define the error out of existence.
- Define errors out of existence versus "never a silent no-op": tension. His fixes
  (idempotent delete, clamped ranges) are new semantics, not skipped errors. He warns it
  is easy to overdo and that important errors must be exposed.
- Masking versus "never tolerate an error": compatible only for expected conditions
  (retry of a lost packet), not for defects.
- Namespace carries the context: extends. "Every word in a name should provide useful
  information" (2nd edition) supports it. The shorter name must still be precise (Vague
  Name red flag). He does not discuss namespace prefixes directly [U].
- Concrete by default, traits only for real polymorphism: mostly agrees ("each piece of
  design infrastructure adds complexity"). Note: his "somewhat general-purpose"
  advice is about the shape of an interface, not about adding a trait.
- Config: extends. Avoid config parameters and compute defaults (pull complexity down).
- Performance: extends. Measure first, keep special cases off the critical path, and
  prefer deep modules because they cross fewer layers.
- Consistency: extends. Do not change conventions for a "better idea"; enforce them
  with tools and reviews.
- Agent-written code: his strategic mindset and design-it-twice give agents a check
  against tactical patching. Every agent change should leave the design as if the
  change was planned from the start (chapter 16, "Stay strategic") [V1].
