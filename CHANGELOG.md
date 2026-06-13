# Changelog

## [0.1.2](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.1...workspace-portal-v0.1.2) (2026-06-13)


### Features

* add immutable path segments (freeze/thaw) ([27ed10f](https://github.com/dstoc/workspace-portal/commit/27ed10f64a1edaa61e0960b7cdd27f256ef71935))

## [0.1.1](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.0...workspace-portal-v0.1.1) (2026-06-05)


### Features

* add edit command for reworking entries in an editor ([abbeb52](https://github.com/dstoc/workspace-portal/commit/abbeb528a2b5993dbac910c11f88291ee544115f))
* add hard link, copy_file_range, forget, and rename inode cache coherence ([fbf03da](https://github.com/dstoc/workspace-portal/commit/fbf03da9609c5ea2794408495b2552954d6a9420))
* confine FUSE host I/O beneath the entry root with openat2 ([4f4e0bf](https://github.com/dstoc/workspace-portal/commit/4f4e0bfc18db2d8709c4189bcec6d61cf4ab87aa))
* handle SIGINT/SIGTERM in foreground daemon to unmount on exit ([e718d39](https://github.com/dstoc/workspace-portal/commit/e718d391e7b418d6be5daae6929ec5ed55acfd7b))
* mvp ([3da82a0](https://github.com/dstoc/workspace-portal/commit/3da82a05cc2e1d75f47e6a83556fbdeed85da567))
* persist atime/mtime in setattr ([d648a92](https://github.com/dstoc/workspace-portal/commit/d648a9289bc9cac21878d2d8892a76b7af04a7a8))
* remove unimplemented features ([5d474c5](https://github.com/dstoc/workspace-portal/commit/5d474c5e02a0a90795c5944b7f3c72c78315e206))
* report real free space from statfs ([fc98139](https://github.com/dstoc/workspace-portal/commit/fc981397cadcb010d08dcc42e40d1fc336d68bca))
* stop forcing a full fsync in flush ([a36bb0b](https://github.com/dstoc/workspace-portal/commit/a36bb0b9a2da584f4cc38ac03f0233a8abcbfa30))
* support symlink creation ([3ffcc66](https://github.com/dstoc/workspace-portal/commit/3ffcc66a29f3c7ff043bfed6889484b413bf82f6))


### Bug Fixes

* avoid self-referential statfs deadlock on the portal root ([fe1c9e9](https://github.com/dstoc/workspace-portal/commit/fe1c9e9c21df83908e16b16a011344766dea30d3))
* enforce file permissions when mounting with --allow-other ([8bf14d6](https://github.com/dstoc/workspace-portal/commit/8bf14d672ac2812cd1ce048ad23023f392db54d1))
* reference-count inode lookups so FORGET honours nlookup ([54ed375](https://github.com/dstoc/workspace-portal/commit/54ed375c7cc95eed5252123bdd91b6ac36cefddf))
