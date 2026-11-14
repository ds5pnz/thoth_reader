# ADR 0003 — Consistent DB snapshots with `VACUUM INTO`

- Status: accepted (M1) · Requirement IDs: §9.3, AC09
- Date: 2026-11-14

SQLx does not expose the SQLite Online Backup API. `VACUUM INTO` produces a transaction-consistent copy of a live
WAL database (it reads one snapshot), which the spec accepts as "동등한 일관된 DB snapshot". Snapshots are written
as `*.partial` then renamed; the newest 3 routine backups are kept; a backup taken before a failed migration is
renamed `*-recovery.sqlite` and never pruned automatically. The open WAL file is never file-copied.
