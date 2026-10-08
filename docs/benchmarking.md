# How MinWin benchmarks, and what it refuses to claim

MinWin's benchmark is a **repeatable baseline of system-level state**. It is not
a performance benchmark, and this document is the place where that distinction
is spelled out rather than hedged.

## What is measured

| Metric id | Source | Unit | Direction |
|---|---|---|---|
| `memory.available_bytes` | `GlobalMemoryStatusEx` → `ullAvailPhys` | bytes | higher is better |
| `memory.load_percent` | `GlobalMemoryStatusEx` → `dwMemoryLoad` | percent | lower is better |
| `cpu.busy_percent` | `GetSystemTimes`, delta over the interval | percent | lower is better |
| `process.count` | `EnumProcesses` | count | lower is better |
| `service.running_count` | `EnumServicesStatusEx` | count | **none** |

Total physical memory and system uptime are recorded once per run as run
metadata, not sampled, because they do not vary within a run.

### CPU utilisation is a delta, not a reading

Windows exposes cumulative idle, kernel and user tick counts. A single read of
them means nothing. Busy time over an interval is:

```
total_delta = (kernel + user) at t1  -  (kernel + user) at t0
idle_delta  = idle at t1             -  idle at t0
busy%       = (total_delta - idle_delta) / total_delta * 100
```

Kernel time already *includes* idle time, which is why idle is subtracted rather
than added — a detail that is easy to get backwards and is covered by unit
tests.

Because the first read has no interval behind it, the sampler is **primed during
the settling period**. That way sample 0 already has a real value instead of
being discarded.

### Why `service.running_count` has no direction

Fewer running services is not inherently better. It depends entirely on *which*
services. MinWin therefore reports the raw difference and classifies the result
as `Unknown` — it will tell you the number moved by 8 and explicitly decline to
call that an improvement. This is the one metric that exists to demonstrate that
the "unknown" path is real and not decorative.

## Sampling

Default: **10 samples, 1000 ms apart, after a 2000 ms settling period.**
Override with `--samples`, `--interval-ms`, `--settle-ms`.

The settling period exists for two reasons: starting MinWin itself disturbs the
machine, and a user typically runs this immediately after doing something else.
It also primes the CPU sampler.

Before sampling, MinWin prints:

> A running Windows system is never idle, so these readings are affected by
> whatever else the machine is doing.

That is not boilerplate. It is the single largest source of error in these
numbers, and it does not go away with more samples.

## Summary statistics

Per metric, per run: **median, minimum, maximum, interquartile range.**

**Medians, not means.** Idle-system samples are routinely disturbed by one short
burst of background work. A mean absorbs that burst into the headline number; a
median does not. There is a unit test asserting that one sample 50× the others
does not move the reported median.

**Quartiles use linear interpolation between order statistics** — the method R
calls `type 7` and NumPy uses by default. Naming the estimator matters, because
"the IQR" is ambiguous without it. The IQR is `p75 - p25`, and MinWin uses it as
its measure of run-to-run spread.

A metric whose samples all failed to read is **absent from the summary**, not
reported as zero. A sample that failed is stored with a `NULL` value and the
error text.

## Comparison

For each metric present in both runs:

```
delta     = median(current) - median(baseline)
spread    = max(IQR(baseline), IQR(current))
threshold = max(metric.minimum_meaningful_delta, spread)

if either run has fewer than 5 readable samples  -> InsufficientData
else if |delta| <= threshold                     -> NoClearDifference
else if the metric has no direction              -> Unknown (delta reported)
else                                             -> Improved / Regressed
```

The boundary is inclusive on the "no difference" side: a delta exactly equal to
the threshold is not claimed.

### The per-metric floors, and why

| Metric | Floor | Reasoning |
|---|---|---|
| `memory.available_bytes` | 128 MiB | Smaller shifts happen constantly as the standby list and file cache breathe. Reporting them would be noise. |
| `memory.load_percent` | 2 points | `dwMemoryLoad` is itself a rounded approximation. |
| `cpu.busy_percent` | 2 points | Idle CPU on a desktop routinely varies by 1–2 points between any two windows. |
| `process.count` | 3 | One browser tab, one update check, or one shell spawning a helper. |
| `service.running_count` | 2 | Demand-start services come and go; direction is `Unknown` anyway. |

**Using the observed spread as the threshold** means a noisy machine needs a
larger difference before MinWin will call it anything. That is deliberate: it is
the behaviour you want from a tool that must not overclaim. On a quiet machine
the floor dominates; on a busy one the spread does.

### What this is not

- **Not a significance test.** There is no p-value, no confidence interval, no
  distributional assumption, and no claim that the samples are independent (they
  are not — consecutive samples a second apart are correlated).
- **Not a controlled experiment.** Nothing holds background activity constant
  between the two runs. A Windows Update check that starts between them will
  move the numbers more than any change MinWin makes.
- **Not attributable per change.** `benchmark` measures the system before and
  after a whole profile. It cannot tell you which change did what. That is the
  main v0.2 goal.

### Why the raw samples are kept

Every individual reading is persisted in `benchmark_samples`, one row per metric
per sample, with its timestamp — not a JSON blob of pre-computed statistics.

That is the whole reason a better analysis is possible later. Paired tests,
bootstrapped confidence intervals, or outlier-robust estimators can be applied
retroactively to runs recorded by v0.1, because the data was not thrown away in
favour of a median.

## Why there is no boot-time measurement

This project's description mentions startup time. MinWin does not measure it.

Windows does expose boot timing: the
`Microsoft-Windows-Diagnostics-Performance/Operational` event log records
event 100 after each boot, with a `BootTime` field. MinWin does not read it,
for four reasons:

1. **The channel is not reliably enabled.** It is disabled on some
   configurations and by some management policies, so the metric would be
   present on one machine and absent on another with no explanation the user
   could act on.
2. **Reading it requires elevation**, which would make `minwin benchmark` an
   administrator-only command — a bad trade for one metric.
3. **The figure is dominated by things MinWin does not touch.** Reported boot
   time is driven largely by third-party startup software and driver
   initialisation. Attributing a change in it to a MinWin service start-type
   change would be unjustifiable.
4. **It is not comparable within a session.** You get one reading per boot, so
   there is no sampling, no spread, and no way to tell a real change from the
   variance of a single measurement.

A number that behaves like that is worse than no number, because it invites
exactly the kind of claim this project exists to avoid. It stays absent until
MinWin can measure it in a way it can defend.

## Reading the output

```
Metric                      Median         IQR   Range
Available RAM             12.53 GB        3 MB   12.50 GB - 12.53 GB
CPU activity                  7.9%        2.4%   5.7% - 9.3%
Process count                  340           0   340 - 342
```

A large IQR relative to the median means the machine was busy and the run is
not a good baseline. Re-run it when things are quieter.

```
Available RAM
  +64 MB median - difference within measurement noise
  (MinWin would need more than 128 MB before calling this a change)
```

MinWin shows the threshold it tested against, so a quiet verdict can be
explained rather than just accepted.
