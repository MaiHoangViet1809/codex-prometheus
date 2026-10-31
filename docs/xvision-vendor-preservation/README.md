# XVision vendor checkout preservation

These artifacts preserve the XVision vendor checkout's uncommitted work at
baseline `f50ceeda2296b4eca41032cc4aa852c148193ecd`. The complete binary-capable
Git diff records its seven dirty tracked files; `Cargo.lock` is also preserved
byte-for-byte. This is historical evidence, not an instruction to apply the
old patch or lock to the canonical checkout. Canonical native context support
is independently implemented and tested and is not claimed byte-identical.

Reconstruction requires the baseline Git objects and applying
`f50ceed-dirty.patch` to that baseline. The lock copy can be compared directly
with the reconstructed `codex-rs/Cargo.lock`; application to another revision
is not guaranteed. Keep these artifacts visible and intact while the old
checkout's removal is under consideration.

At inventory, vendor HEAD and its only local branch `codex/xvision-mvp` were
`f50ceeda2296b4eca41032cc4aa852c148193ecd`; the annotated
`rust-v0.163.0-alpha.10.2` tag object was
`d9151c94a70013f176ff370fa93da02734662e41`. Vendor reflogs contained commit
IDs `4ebc485ca321243727539187169425fcf972f2c4` and
`f50ceeda2296b4eca41032cc4aa852c148193ecd`. The canonical checkout contains
these commits and the exact tag object under its own refs; vendor
`git fsck --no-reflogs --unreachable` reported no unreachable objects. Raw Git
config, remote URLs, FETCH_HEAD and reflogs were not copied; this documents
source-history evidence, not preservation of any independent integration.
