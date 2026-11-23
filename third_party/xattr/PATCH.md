# Vendored xattr 0.2.3

Posthaste does not use extended attributes directly. This copy exists because
gpui depends on `zed-async-tar`, which depends on `xattr ^0.2` with its
`xattr` feature enabled, and xattr 0.2.3 no longer compiles against
current `libc` on Linux:

```
error[E0425]: cannot find value ENOATTR in crate `libc`
  --> xattr-0.2.3/src/sys/mod.rs:12:56
```

`libc` had kept `ENOATTR` as a deprecated alias for `ENODATA` on Linux and
Android and removed it in 0.2.190. xattr 1.x fixed the call site but is a
breaking release, so `zed-async-tar`'s `^0.2` requirement cannot resolve to
it. The one change from 0.2.3 is in `src/sys/mod.rs`, where `ENOATTR` now
maps to `libc::ENODATA` on Linux and Android and to `libc::ENOATTR`
elsewhere.

The workspace root selects this copy with:

```toml
[patch.crates-io]
xattr = { path = "third_party/xattr" }
```

Remove the patch, the manifest entry, and this directory once a `gpui`
release moves `zed-async-tar` to xattr 1.x. `docs/upgrading-gpui.md`
reminds the next upgrade to check it.
