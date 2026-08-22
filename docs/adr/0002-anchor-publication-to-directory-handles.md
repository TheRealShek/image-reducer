# ADR 0002: Anchor Publication to Directory Handles

## Status

Accepted

## Context

Image Reducer publishes files while other processes may rename files or replace directory entries. Path-based temporary files and a later path-based rename would re-resolve mutable directory components, creating source-change and symbolic-link races.

Replacement also needs to prove that the source displaced by publication is the exact inode inspected before processing. A check followed by a normal rename cannot provide that guarantee because the source entry can change between those operations.

## Decision

Publication is rooted in file descriptors opened for the validated source and output directories. Descendant directories are opened one component at a time with `openat`, `DIRECTORY`, and `NOFOLLOW`; missing preservation directories are created with `mkdirat`. Temporary files use process-unique names, `CREATE | EXCL | NOFOLLOW`, mode `0600`, and the already-open destination directory. Security does not depend on name secrecy: exclusive creation and the anchored directory handle prevent redirection or clobbering.

Preservation uses `renameat2(RENAME_NOREPLACE)`. Replacement uses `renameat2(RENAME_EXCHANGE)`, validates the displaced inode against the inspection fingerprint, and rolls the exchange back when it differs. The directory is synchronized while the displaced source remains linked. Only after the replacement is durable is the old source unlinked and the cleanup synchronized.

Ownership is applied before final permission bits because `fchown` may clear set-user-ID or set-group-ID bits.

## Consequences

- Publication cannot escape through a substituted symbolic-link directory.
- Replacement never deletes an unverified source inode.
- A durability failure before acceptance can be rolled back while both versions remain linked.
- Cleanup failure after a durable replacement is reported as a warning; it cannot lose the source or expose a partial candidate.
- The implementation uses a small destination-local temporary-file guard instead of `tempfile::NamedTempFile`, because `tempfile` path APIs do not preserve the anchored directory-handle invariant needed here.
- The initial implementation remains Linux-specific, consistent with the v1 platform scope.
