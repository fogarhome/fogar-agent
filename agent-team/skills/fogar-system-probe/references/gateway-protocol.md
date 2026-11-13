# Gateway side: consuming the load state safely

Design proposal [unverified until implemented]. Principle: **the absence of a fresh
`libre` is a "no"**. Every ambiguity resolves to not scheduling.

## 1. Gateway view of a host

```text
effective_state(host) =
  no report ever                       → desconocido
  now - observed_at > ttl_s            → desconocido   (stale)
  confidence < 0.6                     → desconocido   (gateway-side floor)
  else                                 → reported state
schedulable(host) = effective_state == libre && capacity.cores ≥ job.cores
```
Never infer `libre` from: SSH reachability, low ping, a previous `libre`, a gateway-side
`uptime`/load over SSH, or a missing field.

## 2. Lease protocol (agent is the authority)

```text
gateway → agent : LeaseRequest{job_id, cores, mem_mb, est_duration_s, priority, preemptible}
agent   → gateway: LeaseGrant{lease_id, cores, expires_at (≤ 5 min), renew_every_s}
               | LeaseDeny{state, reason, retry_after_s}
gateway → agent : LeaseRenew{lease_id} every renew_every_s (e.g. 60 s)
agent   → gateway: LeaseRenewed{expires_at} | LeaseRevoked{reason, grace_s}
```
- The agent grants only if its *own* classifier says `libre` with capacity — the
  gateway's cached view is just a pre-filter (avoids TOCTOU between report and start).
- Leases are short and must be renewed; the agent **revokes** when it leaves `libre`
  (ocupado → stop new work, `grace_s` long; no_molestar → pause now, `grace_s` short).
- Revocation actions on the Mac (agent-side, configurable): lower priority
  (`taskpolicy -b -p <pid>` [secondary]), SIGSTOP/SIGCONT the job's process group, or
  SIGTERM after grace. Jobs should be checkpointable/idempotent.
- If renewals stop (gateway gone), the agent lets the lease expire and applies the same
  revocation policy — no orphan load.
- Account delegated CPU per lease (classifier.md §6) so the job does not count as
  "foreign" load.

## 3. Backoff

```text
on Deny / ocupado:     wait = max(retry_after_s, base·2^n) with full jitter; base 30 s, cap 30 min
on no_molestar:        wait = max(retry_after_s, 15 min), or until a push "state=libre"
on desconocido/stale:  wait = 5 min with jitter; alert after 1 h unknown
reset n on successful grant
```
Prefer **push** (agent publishes transitions) + **pull fallback** (heartbeat TTL).
Global admission: the gateway also caps concurrent leases per host (e.g. 1 on a fanless
Air) regardless of advertised capacity.

## 4. Why not just let the gateway SSH in and run `uptime`/`top`?

- Load average lags minutes and ignores memory/thermal/power/user signals.
- Running `top`/`ps` over SSH itself costs CPU and wakes the machine.
- The user's own policy (DND toggle, require-AC) only exists on the agent.
