# Architecture

This document is aimed at another engineer reading the code for the first time.
It answers three questions: how is the system safe, how are changes recorded,
and how does rollback work?

## Layering

```
cli/        argument parsing, confirmation prompts, rendering    (prints)
engine/     business logic, returns structured reports           (never prints)
changes/    what a reversible change IS, and the four supported
benchmark/  collectors, order statistics, conservative comparison
profiles/   TOML parsing and validation
state/      SQLite schema, migrations, persistence
sys/        THE ONLY CODE THAT CALLS WINDOWS, behind traits
core/       errors, time, MinWin's own paths, the single-instance lock
```

Dependencies point downward only. Two rules make the rest of the design
possible:

1. **No module above `sys/` contains a Windows API call.** Everything talks to
   the traits in `sys/traits.rs`.
2. **No function in `engine/` prints anything.** Each one returns a
   serialisable report struct; `cli/render.rs` turns it into text.

## The `sys` seam

`sys/traits.rs` defines the entire vocabulary MinWin has for talking to Windows:

```rust
pub trait Machine: Send + Sync {
    fn info(&self) -> &dyn SystemInfo;
    fn services(&self) -> &dyn ServiceManager;
    fn registry(&self) -> &dyn RegistryStore;
    fn power(&self) -> &dyn PowerManager;
    fn processes(&self) -> &dyn ProcessInspector;
    fn cpu_sampler(&self) -> Result<Box<dyn CpuSampler>>;
}
```

Two implementations exist:

- `sys/windows/` — the real machine. The only place in the crate that calls
  Win32.
- `sys/fake.rs` — `FakeMachine`, which records every mutation in order and can
  be scripted to fail.

`sys::live_machine()` is the only constructor the CLI uses. A fake is never
reachable from a production path.

This seam is why the test suite can exercise apply, verify, diff, rollback,
verification mismatch, access-denied failures and mid-write crashes without
touching the developer's Windows installation — and why several tests can assert
`machine.writes().is_empty()` as a hard guarantee rather than a hope.

The trait surface is also deliberately narrow. `RegistryStore` offers *read a
value, write a DWORD, delete a value* — there is no "write whatever this string
says" entry point, because the only callers are change implementations holding
`&'static str` key paths.

`ServiceManager::set_start_type` changes **only** the start type; every other
`ChangeServiceConfig` field is `SERVICE_NO_CHANGE` or null, so MinWin cannot
corrupt a binary path, service account or dependency list even by accident. It
also rejects kernel-stage start types outright.

### Windows error handling

Every Win32 failure is wrapped with the operation MinWin was attempting, in
MinWin's own vocabulary:

```
Failed to read service configuration for "SysMain": Access is denied. (Windows error 5)
```

Not `operation failed`. `core/error.rs` has no `From<windows::core::Error>`
impl on purpose — conversion goes through `from_win32(operation, error)`, which
forces every call site to name what it was doing.

Buffer sizing is a recurring source of real bugs here. `QueryServiceConfig*`
reports an undersized buffer with `ERROR_INSUFFICIENT_BUFFER` while
`EnumServicesStatusEx` uses `ERROR_MORE_DATA`; both are handled, and service
enumeration retries with a larger buffer rather than reporting a truncated count
as the whole truth.

## The change contract

`changes/mod.rs` defines `SystemChange`. The stages are separate methods
because MinWin needs to run some without the others:

| Method | Question | Writes? |
|---|---|---|
| `check_applicability` | May MinWin touch this on *this* machine? | no |
| `inspect` | What is the current value? | no |
| `plan` | What will MinWin set, and how is it undone? | no |
| `apply` | Perform the write. | **yes** |
| `verify` | Does the machine read back as intended? | no |
| `rollback` | Restore the captured original. | **yes** |

`--dry-run` runs the first three and stops. `diff` uses only `inspect`.
`rollback` calls `rollback` and `verify` in a process that never called `plan`.
That is why these are not one `apply()` with flags.

### Applicability is not privilege

`check_applicability` answers "is this change meaningful and appropriate here" —
the service exists, the Windows build is validated, the device is not centrally
managed, the registry value is not owned by something else.

It deliberately does **not** check elevation. Inspecting a service's
configuration or reading a registry value needs no privileges, so MinWin can
build a complete plan from an ordinary terminal. The planner then separates
`planned` from `blocked_on_elevation` using `metadata.requires_admin`.

This is what makes `--dry-run` genuinely useful: the preview does not depend on
how the terminal was launched. Only one of the four changes
(`power.active_plan_high_performance`) needs no elevation to *apply*, and MinWin
reflects that rather than demanding admin for everything.

### `ObservedState`: human text and machine detail are separate

```rust
pub struct ObservedState {
    pub summary: String,            // for humans
    pub detail: serde_json::Value,  // for comparison
}
```

Diff and rollback compare `detail` by equality and **never** parse `summary`.
Rewording a user-facing string therefore cannot alter MinWin's logic. There are
tests asserting both halves of this: identical detail with different wording
compares equal, and identical wording with different detail does not.

### Three change kinds, not four copy-pasted modules

- `service_start_type.rs` — parameterised by service name and target. Both
  service changes are `const` declarations.
- `policy_dword.rs` — a documented Group Policy DWORD.
- `power_plan.rs` — the active power scheme.

Adding a validated service change is a declaration in the registry plus an
applicability rule, not another 200 lines.

### The `Option<u32>` that matters

`policy_dword.rs` stores state as `Option<u32>`, where `None` means *the value
does not exist*. A policy value that was never configured means Windows uses its
own default. Restoring it therefore means **deleting** it, not writing zero —
writing zero would leave the machine holding an explicit policy it never had, a
silent permanent change disguised as a rollback.

This is the single most important correctness detail in the change layer, and
the integration test asserts the value is absent after rollback, not zero.

## State and crash safety

`state/migrations.rs` holds the schema as append-only versioned migrations. A
database written by a newer MinWin is refused rather than opened.

Benchmark samples are one row per metric per sample, not a JSON blob, so raw
data stays queryable. Change state *is* JSON, because its shape genuinely varies
by change kind; everything around it — ids, statuses, timestamps, ordering — is
in real columns.

### The ordering guarantee

`Database::begin_apply_session` writes the session **and every change's
pre-change state, planned state and rollback data** in one transaction, and
returns only after it commits. `engine/apply.rs` cannot reach a mutation path
without that transaction having completed.

Then, per change:

```
mark Applying  (persisted)  ->  write  ->  verify  ->  record outcome (persisted)
```

So the statuses mean something precise:

| Status | Meaning | Restorable? |
|---|---|---|
| `Planned` | recorded, nothing written | no |
| `Applying` | write issued, outcome unknown — **MinWin was interrupted** | **yes** |
| `Applied` | write succeeded, not yet verified | yes |
| `Verified` | write succeeded and reads back correctly | yes |
| `Failed` | write or verification failed | no |
| `AlreadyCompliant` | machine already matched; nothing written | no |
| `RolledBack` | restored | no |

`Applying` is treated as restorable because the rollback data was persisted
*before* the write. `minwin status` reports any session still `InProgress`, and
there is a test that simulates a mid-write crash and then rolls it back
successfully. A future `minwin recover` needs no schema change.

### Verification failure is failure

If a write reports success but the machine does not read back as intended,
`VerificationResult::Mismatch` is recorded and the change is marked `Failed` —
not warned about. `apply` then **stops**, rather than stacking further changes
on top of a state MinWin no longer understands. Changes already applied stay
applied and stay recorded, so rollback can undo them.

## Diff

For each recorded change, inspect the machine now and compare against both the
state MinWin applied and the state before it:

| Current value matches | Status |
|---|---|
| what MinWin applied | `UnchangedSinceApply` |
| what was there before | `RevertedOutsideMinWin` |
| neither | `ChangedOutsideMinWin` |
| could not be read | `UnableToInspect` |

This exists as its own command because the answer matters before rollback is
even considered. A value somebody else changed is a value MinWin should not
quietly overwrite.

## Rollback

1. Find the most recent session with restorable changes.
2. Inspect the machine **now**, per change.
3. Classify: `SafeToRestore`, `AlreadyOriginal`, `ChangedOutsideMinWin`,
   `UnableToInspect`.
4. Return that assessment for confirmation.
5. Restore in **reverse application order**, verify by reading back, and record
   the outcome — including the state immediately before each restore, so the
   history explains the decision.

A failed restore does **not** stop the run, unlike apply: each remaining change
is an independent chance to get the machine closer to where it started. A failed
or skipped change keeps its `Verified` status, so it stays on record as still
needing a rollback.

### `--yes` cannot bypass a safety condition

`--yes` answers the ordinary confirmation. Restoring a value that changed
outside MinWin is a different decision — "I accept discarding somebody else's
change" — and requires `--allow-external-changes`. `RollbackAuthorisation` is a
separate struct precisely so the two cannot be conflated at a call site, and a
CLI test asserts that `--yes` alone does not set it.

## Concurrency

`core/lock.rs` opens a file in MinWin's data directory with `dwShareMode == 0`,
i.e. exclusively. `apply` and `rollback` hold it across all mutation; `status`,
`benchmark` and `diff` do not take it.

Two concurrent applies would be a correctness disaster, not just a race: the
second would record the first's changes as its own "original" state and silently
destroy rollback. The kernel releases the handle on process exit, including on a
crash, so MinWin cannot leave a stale lock requiring manual cleanup.

## External process execution

`sys/command.rs` exists and **no v0.1 change uses it.** Everything in scope is
covered by a documented Win32 API, which is the better option: structured
results, no localised console output to parse, no shell.

It is there because the next validated changes (scheduled-task state, DISM
capability queries) have no clean API equivalent, and when that day comes there
must be exactly one place in the codebase that spawns a process. It enforces:
resolution to an absolute path under `%SystemRoot%\System32` from a fixed
allow-list (so `PATH` cannot substitute a binary), arguments passed as a vector
and never concatenated, no shell, no inherited stdin, and logging that records
the executable, arguments and exit code but never the environment.

## Profiles as a menu, not a language

A profile entry has two meaningful fields: `id` and `enabled`. There is no field
for a command, script, registry path, service name or file, and
`deny_unknown_fields` means an unrecognised key fails the whole file.

A hostile profile's entire capability is "turn one of four known changes on or
off". The only route from a profile string to executable behaviour is
`ChangeRegistry::require`, which accepts registered ids and nothing else.

The shipped profiles are embedded with `include_str!` from the files in
`profiles/`, so the repository and the binary cannot disagree.

## Where a GUI would attach

`engine/` already returns everything a UI needs, serialisable:

| Command | Function | Report |
|---|---|---|
| status | `engine::status::status` | `StatusReport` |
| benchmark | `engine::bench::run_benchmark` | `BenchmarkReport` |
| apply (preview) | `engine::apply::plan` | `ApplyPlan` |
| apply (execute) | `engine::apply::execute` | `ApplyOutcome` |
| diff | `engine::diff::diff_latest` | `DiffReport` |
| rollback (preview) | `engine::rollback::plan_latest` | `RollbackPlan` |
| rollback (execute) | `engine::rollback::execute` | `RollbackOutcome` |

`cli/render.rs` consumes exactly these and nothing else, which is the evidence
that the split holds. `--json` emits the same structs.

Note that `plan` and `execute` are separate for both apply and rollback, so a UI
can show a plan, let the user look at it, and execute the *same* plan — rather
than re-planning and risking applying something that was never displayed.
