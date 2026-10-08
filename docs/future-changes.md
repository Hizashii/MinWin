# Future change candidates, and why they are not in v0.1

Every change in MinWin has to clear five conditions:

1. Microsoft documents the mechanism.
2. MinWin can read the current value.
3. MinWin can write the new value.
4. MinWin can read it back to confirm it took.
5. MinWin can restore the original **exactly**, including when the original was
   "no value at all".

This file records candidates that did not clear all five, with the specific
reason. It exists so that the shipped set being small is a visible decision
rather than an omission — and so that the reasoning is reviewable instead of
living in somebody's head.

---

## Rejected on evidence

### `DisableWindowsConsumerFeatures` — "Turn off Microsoft consumer experiences"

- **Mechanism:** `HKLM\SOFTWARE\Policies\Microsoft\Windows\CloudContent` →
  `DisableWindowsConsumerFeatures` (DWORD). Stops Windows silently installing
  suggested and promoted apps.
- **Why it looked good:** real background work (downloads and installs the user
  did not ask for), trivially reversible, nothing to do with security.
- **Why it was rejected:** Microsoft's Policy CSP reference documents the
  corresponding `Experience/AllowWindowsConsumerFeatures` policy as **not
  supported on Windows 11 Pro**, and does not list Home at all. MinWin could
  write the value, but on the most common consumer SKU it could not honestly
  claim to know what the value does.
- **What would change the decision:** official documentation confirming the
  behaviour on Pro, or a reliable way for MinWin to detect at runtime whether
  the policy is honoured on this SKU. An applicability check gated on edition
  would then make it shippable.

### Temporary-file and cache cleanup

- **Why it was rejected:** deletion is not reversible. MinWin's rollback model
  promises the original state can be restored, and it will not offer a rollback
  it cannot honour.
- **What would change the decision:** nothing about the current model. If
  cleanup is ever added it belongs in a **separate category** with its own
  vocabulary — "irreversible", explicit per-run confirmation, and no presence in
  the apply/rollback session model at all. It must never appear in a profile
  alongside reversible changes as though it were one of them.

---

## Plausible, not yet validated

### Scheduled task: `Microsoft Compatibility Appraiser`

- **Mechanism:** `\Microsoft\Windows\Application Experience\Microsoft
  Compatibility Appraiser`, disabled via Task Scheduler.
- **Why it is interesting:** it is a genuine, periodic CPU and disk consumer
  that exists to gather compatibility telemetry.
- **Blocking issues:** some tasks under `\Microsoft\Windows\` are protected such
  that an Administrator cannot modify them (they require SYSTEM), so MinWin
  cannot yet promise condition 3 on every machine. Task state also needs a
  different inspection path from services and registry values.
- **Next step:** implement task-state inspection, confirm on several machines
  whether an elevated Administrator can reliably toggle it, and add an
  applicability check for the case where it cannot. This is the change that
  would first put `sys::command::WindowsCommandRunner` to real use.

### `WSearch` — Windows Search indexing

- **Mechanism:** service start type, same as the two shipped service changes.
- **Why it is deferred:** the tradeoff is large and very user-visible. Start
  menu search, Settings search, File Explorer search and Outlook search all
  degrade. That is a legitimate choice for some users, but it is not something
  to put in a profile called "minimal", and MinWin cannot currently measure the
  benefit to set against the cost.
- **Next step:** offer it as a standalone opt-in change with a blunt tradeoff
  description, the way `memory.sysmain_start_type` is offered now, once
  per-change measurement exists.

### Game DVR background recording

- **Mechanism:** `HKCU\System\GameConfigStore` → `GameDVR_Enabled`, the same
  value the Settings toggle writes.
- **Why it is deferred:** when "record what happened" is already off, the
  measurable saving is close to nothing, so MinWin would be making a change it
  cannot justify. It also risks reading as the indiscriminate Xbox-disabling
  that MinWin's gaming profile explicitly avoids — plenty of people use Game
  Bar and Game Pass.
- **Next step:** only worth adding if per-change measurement shows a real
  effect on machines where recording is enabled.

### Per-user startup entries

- **Mechanism:** `HKCU\...\CurrentVersion\Run`, and the `StartupApproved` keys
  the Settings UI and Task Manager use.
- **Why it is deferred:** MinWin cannot hardcode a list of vendor applications
  to disable — that is exactly the "trust me, disable these 40 things" pattern
  this project rejects. Doing it properly means letting the *user* choose
  entries, which requires typed, validated parameters in profiles. That is a
  real design change to the profile schema and the security argument around it,
  and it does not belong in v0.1.
- **Next step:** design parameterised changes with a validated value domain, so
  a profile can still only express choices MinWin understands.

---

## Rejected permanently

These are out of scope as a matter of principle, not pending evidence. Security
is not bloat.

- Windows Defender, in any form, including Tamper Protection and exclusions
- Windows Firewall
- Windows Update, the service or the policy
- UAC
- SmartScreen
- Memory Integrity / HVCI
- Credential Guard, LSASS protections, Windows Hello
- BitLocker
- Network stack services
- Driver services
- Setting any service to `Disabled` rather than `Manual`
- Anything requiring an undocumented API, a hardcoded struct offset, or GUI
  scraping
- Anything that cannot be read back to verify it took effect
