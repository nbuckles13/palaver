# Palaver — development environment and development flow

Draft for discussion, 2026-11-14.

## Part 1: development environment

### Native cargo, no wrapper

Everything is a standard cargo command, so a developer, an agent and CI run
the same thing on Linux, Windows and macOS with no bash and no custom runner:
Two things beyond the Rust toolchain are needed:

- a C compiler and `cmake`, to build libopus from source;
- on Linux, the ALSA headers, for the sound crate (`palaver-sound`).

On Debian or Ubuntu (WSL2 included):

```
sudo apt install build-essential cmake libasound2-dev
```

Windows and macOS need nothing for sound beyond what the system has. To work without the
ALSA headers, name the crates: `-p palaver-bot` builds no sound code.

| Command | Covers |
|---|---|
| `cargo fmt --check` | Formatting (plain `cargo fmt` locally) |
| `cargo clippy --workspace --all-targets -- -D warnings` | Lints, including the per-crate bans (see Guards) |
| `cargo deny check` | Licences, advisories, duplicate versions |
| `cargo nextest run` | Unit tests, simulation, **guards**, and time budgets (T1–T2) |
| `cargo nextest run --profile e2e` | Local and emulated-network end-to-end (T3–T4) |

- **Guards are tests.** A `guards` crate holds them as ordinary integration tests that inspect the
  workspace (via `cargo metadata` and source scans). They run in the same `nextest` invocation,
  report the same way, and are subject to the same time budgets.
- **End-to-end scenarios are tests too**, in an `e2e` crate that spawns bot binaries. They sit in
  their own nextest profile because they run in real time.
- **Tools to install**: `cargo-nextest` and `cargo-deny` (both cargo plugins), pinned in CI.
- **"Check" below means** the first four commands; **"verify"** means all five. The list lives in
  `CLAUDE.md` and the CI workflow; a guard test asserts the two agree.
- **No git hooks.** Commits are free and instant, so checkpoint commits cost nothing. Nothing
  reaches `main` except through a PR, and CI is the gate. The task skill runs "check" before it
  opens a PR, to avoid wasting a CI cycle, but that is a courtesy, not an enforcement point.
- **`xtask` is deferred.** The one thing that is not naturally a test is driving bots on remote
  machines (T5). A small `xtask` crate (a Rust program in the workspace run via a cargo alias) is
  added for that when it is needed, and not before.

Rules carried over from dark_tower because they were cheap and paid off: a step that did not run
counts as failed; flaky tests are never retried; on failure, print where the log is.

### Tiers

| Tier | What it proves | Where it runs | How | Budget |
|---|---|---|---|---|
| **T0 build** | Compiles, formatted, lint-clean, dependency rules hold | everywhere | fmt, clippy, `cargo deny` | seconds–1 min incremental |
| **T1 unit** | Pure logic: codec, khef behaviour, topology, control state machines | everywhere | `cargo nextest run`: unit tests, property tests, test vectors, guards | per test: see Time budgets |
| **T2 simulation** | Whole-meeting behaviour: join/leave/crash/partition, non-full-mesh graphs, conductor hand-over, loudest-N convergence | everywhere, in-process | Deterministic simulator on virtual time; a failing seed replays exactly (`SIM_SEED=N cargo nextest run <test>`) | per test: see Time budgets; thousands of seeds nightly |
| **T3 local end-to-end** | The real engine: real QUIC, real codecs, synthetic media, N bot processes on loopback | Linux, Windows, macOS | `e2e` profile: tests spawn bots, drive a scenario, assert on their metrics and event logs | per scenario ≤ 30 s; suite ~5 min |
| **T4 emulated network** | NAT classes, blocked pairs, loss, jitter, bandwidth caps; hole punching; khef selection under bad links | Linux only | Same `e2e` crate, with bots in network namespaces (nftables NAT, `tc netem`); unprivileged via user namespaces or inside the dev container | ~10 min; PR on Linux, full matrix nightly |
| **T5 distributed** | Real networks and real machines | several hosts | Same bot binary on each host, started/stopped/scripted over SSH with logs pulled back. Also the manual mode: "join my meeting with 20 bots" | nightly or on demand |
| **T6 native media** | Real cameras, microphones, speakers, echo cancellation, GPU rendering, UI | Windows host and the MacBook | GUI harness; scripted where possible, otherwise a short manual checklist per milestone | per milestone |
| Fuzz / soak | Parsers survive hostile input; long meetings with chaos stay healthy | Linux | `cargo fuzz`; bots with random leave/crash/throttle | nightly |

### Time budgets

Test time is a first-class constraint, enforced by configuration rather than goodwill.

| Class | Target | Hard limit (test is killed and fails) |
|---|---|---|
| Unit test (T1), guard | < 10 ms typical, < 100 ms required | 1 s |
| Simulation test (T2), integration test | < 1 s | 5 s, and only for tests on an explicit exceptions list |
| End-to-end scenario (T3–T4) | as short as the scenario allows | 30 s |

Simulation tests run whole meetings through the sans-IO cores, so the cores (and the
simulator) are compiled optimised in development and test builds (`Cargo.toml`, per-package
`opt-level = 2`). Overflow checks and debug assertions stay on. Without this a 50-node
meeting takes four times as long to simulate, which is the difference between 0.5 s and 2 s
a test.

One test's time is not ours: the guard on the real dependency graph runs `cargo metadata`
(60 ms warm; once over 5 s on a cold CI runner, cause unknown). It keeps the 5 s limit, with
three measures: both graph guards share one cargo process; CI runs it on Linux only, since
the answer is the same on every platform; and it writes cargo's messages and its own timing
to standard error as they happen, so a slow or killed run shows what cargo was waiting for.

How it is enforced:

- **Hard limits** are nextest configuration (`.config/nextest.toml`): `slow-timeout` with
  `terminate-after` kills and fails any test over its limit, with per-crate overrides for the
  simulation and e2e classes. This applies on every platform.
- **Required targets** (100 ms / 1 s) are checked on Linux CI from nextest's per-test timing
  report: any test over its class budget fails the run. Checked on one reference platform only,
  because shared Windows/macOS runners are noisy and timing failures there would be flaky.
- **Exceptions are a short list in the nextest config**, each with a reason. A guard fails if the
  list grows without a matching marker in the PR, so "very special cases" stay special.
- **Trend, not just threshold**: CI publishes the slowest 20 tests and total suite time per run.

What makes the budgets achievable:

- **Time in code.** A span is a `std::time::Duration`; a point in time is a
  `proto::Timestamp` (microseconds since the node started). No raw integers of milliseconds or
  microseconds, so units cannot be mixed up, and a time minus a time is a span by type. Units
  appear only where the wire needs them. Sans-IO crates are handed the time and never read a
  clock. The async shell (`net`, `sdk`) will read tokio's clock, not the standard one, so tests
  that pause time are not bypassed; a clippy rule for that is added when those crates have code.
- **Virtual time.** The simulator and all async tests run on a paused/virtual clock; a test that
  simulates a five-minute meeting takes milliseconds. Real `sleep` is banned in test code outside
  the `e2e` crate by a clippy `disallowed-methods` rule.
- **Our own simulator.** A discrete-event loop over the sans-IO cores, not a framework that
  runs async code: 60 simulated seconds of a 50-node meeting cost about 140 ms, against seconds
  for turmoil (spike 6). Sans-IO crates must not iterate hash maps in random order (clippy
  disallowed-types), and a self-test requires identical traces for identical seeds.
- **No network or disk in T1–T2.** Sans-IO crates cannot open sockets anyway (layering guard).
- **Bounded property tests.** Case counts are sized to the budget on PR; the large runs are nightly.
- **Real time only in `e2e`.** Anything that needs wall-clock behaviour (QUIC timers, codecs, hole
  punching) lives there, under its own budget.

One caveat: nextest runs each test in its own process, which adds a few milliseconds of start-up
per test (more on Windows). "< 10 ms" is therefore a target for the test body, visible in the
report, rather than an enforceable hard limit.

Notes:

- **T2 is the workhorse.** Most of what is novel in Palaver (plans, migration, crash handling,
  audio selection) is decided by sans-IO code, so it can be judged in-process, deterministically,
  in seconds. This is also what makes an autonomous flow cheap (part 2).
- **T3–T5 share one scenario format and one bot.** A scenario is a script of events
  (join, publish, mute, leave, kill, throttle) plus assertions; the tiers differ only in where the
  bots run and what the network between them is.
- **Media assertions are mechanical.** Synthetic audio carries sequence marks and synthetic video
  carries a frame counter in the pixels, so "zero audio gap across a khef leave", "freeze ≤ X ms
  after a crash", latency and A/V sync are numbers, not opinions.
- **A failed run leaves a run directory**: every bot's event log, metrics and plan history — the
  same diagnostics bundle the product exports.

### Machines

- **Day to day: WSL2** (this machine: 32 cores, 15 GB; Rust 1.95, podman, gh, node present).
  To install: `clang`, `cmake`, `cargo-nextest`, `cargo-deny`, and the audio headers
  (`sudo apt install libasound2-dev`); video dev headers when video arrives. T0–T4 all
  run here. Real devices do not (WSLg audio latency is unusable for echo work; no camera by
  default), which is why synthetic sources are in the engine.
- **Windows native**: build and run on the Windows host of this machine for T3 and T6. Cross-compiling
  from WSL works (`cargo zigbuild --target x86_64-pc-windows-gnu`, including the C parts) and the
  result runs on the host from WSL; a native toolchain on the Windows side is the alternative.
  Smart App Control must be off on the host, or builds signed: it blocks unsigned programs.
- **macOS**: CI runner for T0–T3; the MacBook for T6.
- **GitHub-hosted runners first.** Free for a public repo and they cover Linux, Windows and macOS
  for T0–T3, plus T4 on Linux (runners allow the privileges network namespaces need).
- **T5 fleet later.** Runners are a poor fit for distributed runs: they sit behind the provider's
  NAT, cannot accept inbound connections, and whether two of them can hole-punch to each other is
  unverified. When phase 4 (discovery, NAT) starts, add one small cloud VM with a public address:
  it is the fixed reachable peer for the fleet, and it doubles as a live test of "headless node as
  fallback khef". Home machines (the Windows host, the MacBook) add real NATs.

### CI

GitHub Actions, running the same cargo commands:

- **Per PR**: T0–T3 on Linux, Windows and macOS; T4 on Linux.
- **Nightly**: many-seed simulation, full T4 matrix, fuzz, soak; T5 if a fleet exists.
- Build artifacts (bot and harness per platform) are kept so T6 testers do not need a toolchain.

### Guards

Guards catch what tests are structurally bad at: properties of the *code and repository* rather
than of behaviour. dark_tower's guard set (about 50, backed by a 58k-line binary) falls into
families; most of the bulk serves Kubernetes, dashboards, Rust↔TypeScript duplication and service
ownership, which Palaver does not have. The families and the lessons transfer:

| Family (dark_tower example) | Why a test cannot hold it | Palaver equivalent |
|---|---|---|
| **Leak** (no secrets/PII in logs; `#[instrument]` must `skip_all`; no key material in internal messages) | The leak is in a path no test asserts on | Passwords, `K_auth`, meeting and transmit keys never reach logs, plan/report messages, the diagnostics bundle or a relay network. Names, IP addresses and meeting ids never reach logs or metric labels unredacted — this is the product's privacy promise |
| **Boundary / scope deny** (no log or metric macros anywhere under the media hot-path directory) | Violations work fine and pass every test | Sans-IO crates use no sockets, runtime, clock or ambient randomness. `khef-core` never names a media kind or codec. No logging, blocking or unbounded queues in the forwarding path |
| **Test integrity** (test file not registered so it silently never runs; tests that accept failure as a pass; tests removed or loosened) | The suite stays green while proving less | Same, and it matters more with autonomous agents: the cheapest way to make a failing task pass is to weaken the check. Simulator invariants, scenario assertions and test vectors may not be removed, ignored or loosened without a human-visible marker |
| **Drift** (one rule written in two places must match; codec vs test vectors) | Each side passes its own tests | Mostly avoided by one language and one `proto` crate. Remaining: wire structs vs pinned test vectors, and a wire change without a protocol-version bump |
| **Reference integrity** (doc citations and navigation pointers resolve) | Not code | Pointers in `CLAUDE.md` and docs resolve |
| **Semantic** (LLM-judged: credential flow, blocking in actors, lost error context) | Needs reading for meaning | A short checklist the reviewer agent applies when the diff touches the relevant paths |

**Mechanism, cheapest first.** A rule goes to the first mechanism that can hold it:

1. **Types.** Secret material lives in a wrapper (`proto::Secret`) with no `Display` or
   `Serialize` and a `Debug` that prints a fixed placeholder, so structs holding one can still
   derive `Debug`; addresses and names live in a wrapper (`proto::Redacted`) that redacts by
   default. Reading either is an explicit call (`expose`, `reveal`), which is the audit surface.
2. **Clippy configuration.** Nearly every lint clippy has is on: the `pedantic`, `nursery`
   and `cargo` groups, as warnings, which CI turns into errors.
   - *Off for the workspace*, each with its reason in `Cargo.toml`: `multiple_crate_versions`
     (our dependencies' choice; `cargo deny` watches it), `option_if_let_else` (asks for
     two closures where two branches read better), and `suboptimal_flops` and
     `imprecise_flops` (they ask for `mul_add`, `hypot` and the like, whose results differ
     by machine; the same input must give the same output everywhere). **Do not apply
     clippy's automatic fixes for float arithmetic.**
   - *Anywhere else* a lint is set aside, it is with `#[expect(..., reason = "...")]` on the
     smallest item that needs it, never for a whole crate outside tests. `#[allow]` is
     itself denied: an `expect` that no longer excuses anything is reported, so none
     outlives its cause. Numeric casts are the usual case: a lossless or checked
     conversion where one exists, an `expect` with the reason where the cast is the
     intent.
   - Code is formatted with `cargo fmt` before every commit; CI refuses anything else.

   Further: Deny `unwrap`/`expect`/`panic` outside tests, everywhere, including
   crates that wrap C: every C return code becomes a `Result`. `unsafe` is forbidden outside
   those crates; inside them each `unsafe` block needs a safety comment, there is one small crate
   per C library exposing only a safe API, callbacks invoked from C catch panics, and the
   stricter indexing and arithmetic lints are on (also for the forwarding path); per-crate `disallowed-methods`/`-types`/`-macros` ban `Instant::now`,
   `SystemTime`, thread RNG, unbounded channels and log macros where they must not appear.
3. **Dependency rules.** `cargo deny` for the licence allow-list (how "permissive, no GPL" is
   enforced), advisories and duplicates; a layering check over `cargo metadata` so the sans-IO
   crates cannot depend on a runtime or on `net`, and `khef-core` cannot depend on `media`.
4. **Guard tests** in the `guards` crate. Small Rust checks for what is left: test integrity, wire-version bump,
   reference integrity. Each is one module with its own self-test.
   Also there: **test-only features**. `palaver-proto`'s `fake-crypto` feature compiles a signer
   anyone can forge, for the simulator and tests. No test can notice it switched on in a real
   build, because tests use it too, so a guard over `cargo metadata` checks that only the
   simulator may enable it and that nothing which ships depends on the simulator.
   Unsafe code: the workspace lint is `deny`, not `forbid`, so that the crate wrapping a C
   library can allow it for itself; a guard keeps that to a list of crates (`crates/opus/`),
   fails on the keyword or the allow anywhere else, and fails if a listed crate holds none.
5. **Reviewer checklist.** Only for what needs judgement.

**Rules for guards themselves** (dark_tower learned each the hard way):

- **No vacuous pass.** A guard that scans and finds nothing to check fails; "zero violations in
  zero files" is the bug guards exist to prevent.
- **Scope by directory, not file list.** File lists narrow silently on refactor.
- **Every guard has a self-test** proving it fails on a seeded violation.
- **A guard is added when something escaped** — a bug or review finding that tests could not have
  held — not speculatively. The starting set is items 1–3 plus test integrity.

Also pinned: `rust-toolchain.toml` with one exact version.

## Part 2: development flow

### What dark_tower learned (and paid for)

dark_tower's flow went through five rebuilds. Its own measurements (ADR-0037: one 26-task story
cost about $5,700 and 125 API-hours) show where the value was:

- About 90% of findings that changed shipped code came from **code review**; plan review was a
  rubber stamp about 80% of the time.
- The most valuable catches came from reviewers who **executed something** (mutated code, deleted a
  line to prove a test fails), not from reading.
- Rules stick when **tooling enforces them**; rules in prompts drift.
- The control flow must be **deterministic**: a model orchestrating its own steps skips them.
- The verdict must be **re-checked independently**, not taken from the agent that did the work.
- Accumulated "lessons learned" files went stale and overrode real decisions; **pointer-only**
  navigation backed by a dead-link check replaced them.
- The per-service ownership model was measured as mostly ceremony.

Much of the remaining machinery exists because dark_tower is a multi-service server system:
15 specialist agents, cross-service ownership rules, a Kubernetes test cluster with a helper
daemon, operations/observability reviewers, a 58k-line guard binary, a 2,600-line story runner,
36k lines of pipeline shell. Palaver is one workspace with headless-testable behaviour and needs
none of that.

### The flow

```
design doc / short ADR ──► milestone ──► task list ──► per task:
                                                         implement (+ tests)
                                                         check                (fast, T0–T2)
                                                         review               (fresh context, executes)
                                                         verify               (adds T3; T4 if net touched)
                                                         commit ──► PR ──► CI ──► merge
```

**Work definition**

- **Milestone** = a demo from the plan (e.g. "voice call from a pasted link").
- **Task** = the largest demonstrable increment that fits one session, with acceptance stated as
  something executable wherever possible: a simulator invariant, an e2e scenario, a test vector.
  Tasks live in one markdown file per milestone; a checkbox list, nothing more.
- **ADR** only for a real fork (wire format, plan format, crypto, a dependency we will be stuck
  with). One agent writes it with an explicit "argue the alternatives" section; no debate team.
  Everything else is decided in the PR.

**Roles (three, not fifteen)**

| Role | Does |
|---|---|
| Implementer | One agent session per task. Writes code and tests, runs the check commands. |
| Reviewer | One separate agent with fresh context, given the diff and the task, not the implementer's reasoning. Must execute: break the code and confirm a test fails, run the scenario, try the negative case. Findings are fixed now, not deferred. A security lens is added only when the diff touches crypto, parsing or admission (decided by paths, mechanically). |
| Human | Chooses milestones and design forks, reviews ADRs, runs T6 and the milestone demo, merges. |

**Enforced, not instructed**

- The task loop is a single skill (target: ~150 lines) with a fixed step order.
- Verify is re-run by CI on the PR, which is the independent check; nothing trusts an
  agent's "it passes".
- `CLAUDE.md` stays under ~80 lines: how to build and test, the principles, pointers to the docs.
  A tiny check fails if a pointer is dead. No lessons-learned files; facts live in code, docs and
  ADRs.
- Plan review happens only for tasks flagged as touching a wire format or adding a seam.

**Things deliberately not built**

Specialist rosters, ownership rules, per-task output records beyond the PR description, a custom
guard binary, a story-runner, a deferred-work ledger (a short `TODO.md` at most).

### Growing autonomy in stages

Autonomy is only as safe as the checks that judge the result, so it follows the test tiers.

| Stage | When | How work runs |
|---|---|---|
| **0 — paired** | Phase 0 spikes | Interactive sessions. Spikes are exploration and decisions; nothing to automate. |
| **1 — supervised tasks** | From phase 1, once T0–T2 exist | Human starts a task; the skill runs implement → review → verify → PR; human merges. |
| **2 — unattended milestones** | Once T3 exists and stage 1 has run cleanly for a while | A small loop works through a milestone's task list in a container, one PR per task, stopping on the first task it cannot verify. Human reviews the batch and the demo. |
| **3 — auto-merge for low-risk tasks** | Optional, later | Tasks not touching wire formats, crypto or the plan format merge on green CI. |

**Container (needed from stage 2):** a plain Containerfile with the toolchain and the agent CLI,
run with podman against a fresh clone, with no git or cloud credentials inside; the host pushes
and opens the PR. It doubles as the place T4 runs with network capabilities. No helper daemon.

**Keep a ledger from the start:** one line per task with cost, review findings that changed code,
and verify failures. dark_tower could only trim its process once it had those numbers.

### Decisions

- Public GitHub repo, created when ready; PRs and Actions.
- Human merge for now; auto-merge for low-risk tasks eventually.
- Spikes stay local and are thrown away; only their written conclusions are kept.
- GitHub-hosted runners first; a cloud VM when distributed/NAT testing starts.

- Native cargo commands as the entry point; guards and end-to-end scenarios are tests; `xtask`
  only when remote-fleet orchestration needs it.
- Per-test time budgets enforced by nextest configuration and CI.
- A MacBook is available for native media testing.
