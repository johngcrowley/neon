# Resuming the LFC across compute restarts

The local file cache (LFC) is the compute's disk-backed page cache: every page
the neon SMGR reads from or writes toward the pageserver is copied into a
single large file, addressed through a shared-memory hash of 1 MB chunks. It
is what stands between a warm working set and a storm of getpage requests.

Until now the LFC died with the postmaster. `LfcShmemInit()` opened the cache
file with `O_TRUNC` on every startup — necessarily so, because the mapping
from file offsets to buffer tags lived only in shared memory. A pod restart
meant an empty cache, and the first minutes of a restarted compute were spent
re-downloading its working set from the pageserver, even when the cache file
sat intact on a persistent volume the whole time.

This change makes the mapping survive. With `neon.file_cache_resume = on`,
the postmaster dumps the chunk map to `<neon.file_cache_path>.resume` as it
exits after a clean shutdown, and the next incarnation restores it instead of
truncating — the compute comes back with its cache exactly as warm as it left.

## Enabling it

Two knobs, either works:

1. **compute_ctl flag**: pass `--preserve-lfc` to `compute_ctl`. It appends
   `neon.file_cache_resume=on` to the generated `postgresql.conf`, after the
   spec settings so the flag always wins.

2. **GUC directly**: set `neon.file_cache_resume = on` wherever the compute's
   GUCs come from — the compute spec's `cluster.settings`, or the endpoint's
   `postgresql.conf` under `neon_local`.

The one hard requirement is that `neon.file_cache_path` points at storage
that survives the restart — a persistent volume, a host-path zpool, anything
outside pgdata. compute_ctl wipes and rebuilds pgdata from a fresh basebackup
on every start, so a cache inside it can never be resumed (the flag logs a
warning if it can detect this; the result is just a cold start, never an
error).

The GUC is `PGC_POSTMASTER` and defaults to `off`, so nothing changes for
deployments that don't opt in.

## What gets written, and when

At postmaster exit — after every child process is gone, via an
`on_shmem_exit` callback registered in `LfcShmemInit()` — the extension
writes a small state file next to the cache:

```
LfcResumeHeader                 136 bytes
  magic, version, pg major, BLCKSZ, chunk_size_log
  n_entries, size_chunks
  system_identifier               from pg_control
  end_of_wal                      normalized WAL insert position
  tenant_id, timeline_id          from the neon.* GUCs
  crc                             CRC-32C of the entry array
LfcResumeEntry × n_entries      40 bytes each
  key                             BufferTag of the chunk's first block
  offset                          chunk index in the cache file
  bitmap                          which of the 128 blocks are AVAILABLE
```

Entries are emitted in LRU order, oldest first, so the restored cache also
inherits its eviction order. The file is written to a temp name, fsynced,
renamed into place, and the directory fsynced — it either exists whole or not
at all. Before writing it, the cache file itself is fsynced, so the map never
promises bytes the kernel hasn't persisted.

The dump is skipped entirely unless the shutdown was clean: the callback
reads `pg_control` and requires `state == DB_SHUTDOWNED`. That check is
load-bearing. A clean shutdown means the shutdown checkpoint has flushed
every dirty buffer through the SMGR — and neon's SMGR write path copies each
of those pages into the LFC — so at that moment the cache holds the latest
version of every page it contains. After a crash it may not, and no state
file is written; the next start is cold, which is exactly the old behavior.

## What gets checked on the way back in

`LfcShmemInit()` runs in the postmaster with freshly created (empty) shared
memory. If the resume GUC is on it calls `lfc_try_resume_state()`, which
reads the state file and then **unlinks it durably before anything else** —
the file is single-use, and a crash later in startup must never replay an old
map against a cache file that has since been written to.

Then the gauntlet, any failure of which means a cold start and a one-line LOG
explaining why:

- magic, version, Postgres major, `BLCKSZ`, and chunk size must match the
  running binary's configuration, and the CRC must verify;
- `system_identifier`, `neon.tenant_id`, and `neon.timeline_id` must match
  the fresh basebackup's — a compute (or its cache volume) reattached to a
  different branch, or a PITR onto a new timeline, is refused here;
- the chunk count must fit inside the current `neon.max_file_cache_size` and
  `neon.file_cache_size_limit`;
- the cache file must exist and be no larger than the map says, and every
  block the bitmap promises must lie within the file's actual size;
- every entry's offset must be unique and in range, every buffer tag valid
  and chunk-aligned.

And the crucial one:

- the stored `end_of_wal` must equal `checkPointCopy.redo` in the new
  `pg_control`.

That comparison works because of how the pageserver builds basebackups.
`generate_pg_control()` in `libs/postgres_ffi` always sets the redo pointer
to `normalize_lsn(basebackup_lsn)`. The dump records the postmaster's WAL
insert position at shutdown, normalized the same way. The two are equal if
and only if the basebackup was taken at exactly the WAL position the previous
incarnation shut down at — i.e. not a single record was written to the
timeline in between. Another compute ran against the branch while the pod was
down? WAL advanced, LSNs differ, cold start. The pod simply came back? The
first thing a starting primary does is sync safekeepers and take its
basebackup at that same flush LSN — the LSNs match and the cache resumes.

There is no weaker fallback on purpose. LFC hits return bytes directly, with
no LSN check against the last-written-LSN cache, so a stale page in a resumed
cache would be served silently. Equality is the only comparison that is
always safe.

## Restoring

On success the restore rebuilds what `O_TRUNC` used to destroy: each entry is
re-entered into the shared hash with its saved offset, block-state bits set
`AVAILABLE` per the bitmap, and pushed onto the LRU in dump order. Offsets
the map doesn't cover (chunks that were pinned mid-eviction at dump time, or
punched holes) are re-entered as holes so their file space is reused rather
than leaked. `size`, `used`, `used_pages`, and `limit` are set to match, and
the cache file is left untouched — the next `lfc_ensure_opened()` opens it
without truncation and reads proceed as if the restart never happened.

If any step fails after hash insertion has begun (which a valid CRC makes
effectively impossible), the restore clears everything it entered and falls
back cold. Losing the state file is never a correctness problem, only a
warmth problem.

Two details worth knowing:

- **Unlogged and temporary relations** are immune by construction: the neon
  SMGR routes them to the `md` layer, so their pages never enter the LFC and
  resume cannot resurrect them past the restart-reset semantics.
- **Backend crashes without postmaster death** are fine: shared memory is
  recreated and `LfcShmemInit()` runs again, but the state file was already
  consumed at boot, so the re-init truncates as before. The exit callback is
  registered once and guarded by `IsUnderPostmaster`, so forked children
  exiting never trigger a dump.

## Interaction with autoprewarm

The existing prewarm machinery (`neon.get_local_cache_state()` offloaded to
endpoint storage, `prewarm_local_cache()` on start) solves a different
problem: it re-downloads the working set from the pageserver, which works
even when the cache file is gone or the WAL has advanced. Resume is strictly
cheaper when it applies — no pageserver traffic at all — and the two compose:
if resume succeeds, a subsequent prewarm finds the pages already present and
skips them; if resume falls back cold, prewarm proceeds as today. Belt and
suspenders: `--preserve-lfc` for the common pod-restart, autoprewarm for
everything else.

## Failure matrix

| scenario | outcome |
|---|---|
| clean stop → start, no other WAL | resumed, zero pageserver reads for cached pages |
| clean stop → another compute writes → start | LSN mismatch → cold |
| crash / OOM / `kill -9` | no dump (pg_control not `DB_SHUTDOWNED`) → cold |
| PITR to new timeline, same cache volume | timeline id mismatch → cold |
| cache volume lost | no file → cold |
| PG major upgrade | header mismatch → cold |
| `file_cache_path` inside pgdata | file wiped with pgdata → cold (+ warning) |
| torn/corrupt state file | CRC or shape check → cold |

## Verified

Smoke-tested end to end on an isolated `neon_local` instance (PG 16):

- populated a 128-chunk LFC, clean stop → `LFC: saved resume state ... 128
  chunks, end of WAL 0/2817208`, state file exactly 136 + 128×40 bytes;
- restart (fresh basebackup, pgdata rebuilt) → `LFC: resumed 128 chunks
  (2119 pages) from previous instance at 0/2817208`, state file consumed;
- full-table scan after resume: 2,125 LFC hits, **zero** new misses, and all
  200,000 rows of deterministic data (`pad = md5(id)`) verified correct;
- staleness guard: planted a stale state file after advancing the WAL →
  `LFC: WAL advanced since resume state was saved (0/3430AA0 vs 0/28172F0),
  starting with cold cache`, and the updated rows read back correctly.

The extension compiles clean against v14 through v17.

## Files

- `pgxn/neon/file_cache.c` — GUC, dump callback, restore path, validation
- `compute_tools/src/bin/compute_ctl.rs` — `--preserve-lfc` flag
- `compute_tools/src/compute.rs` — `ComputeNodeParams::preserve_lfc`
- `compute_tools/src/config.rs` — writes the GUC into `postgresql.conf`
