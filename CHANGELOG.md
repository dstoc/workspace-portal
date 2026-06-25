# Changelog

## [0.1.8](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.7...workspace-portal-v0.1.8) (2026-06-25)


### Features

* invalidate FUSE dentries on namespace changes ([34f3dbf](https://github.com/dstoc/workspace-portal/commit/34f3dbf95faf9b981143a3bfcc659402bdbf2ed7))


### Bug Fixes

* stat open unlinked files by handle ([8212bff](https://github.com/dstoc/workspace-portal/commit/8212bffc425544b19afb8f4c5ee1f70f15e1009c))

## [0.1.7](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.6...workspace-portal-v0.1.7) (2026-06-21)


### Features

* add hardlink audit command ([075c0a4](https://github.com/dstoc/workspace-portal/commit/075c0a430fd5d2068b49b894fe2ac0115258fd13))
* add symlink escape audit ([827122f](https://github.com/dstoc/workspace-portal/commit/827122f96a7d2acfb8e62ec6fa24c93c861f0e84))
* restore scoped hard links ([a8328ad](https://github.com/dstoc/workspace-portal/commit/a8328ad078a75f4d74b13c56361d9f6e79f72dc2))

## [0.1.6](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.5...workspace-portal-v0.1.6) (2026-06-21)


### Features

* forget stopped workspace metadata ([84e6383](https://github.com/dstoc/workspace-portal/commit/84e6383195a0ed03954b508aeb14aabb6b04c997))
* remove edit-redundant entry commands ([d0a957a](https://github.com/dstoc/workspace-portal/commit/d0a957a5c64729684cc452d7f41efc70dbe06e52))
* use positional workspace arguments ([1c50816](https://github.com/dstoc/workspace-portal/commit/1c50816f4c5419547fbd881e376d9087bc81b632))

## [0.1.5](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.4...workspace-portal-v0.1.5) (2026-06-19)


### Features

* `edit` now uses toml format ([2b450ad](https://github.com/dstoc/workspace-portal/commit/2b450ad918f3048f2ed3a93b59c8471b896095b9))
* add readlink policy ([e78d836](https://github.com/dstoc/workspace-portal/commit/e78d836051866f4680929f6a228a6e134791350d))

## [0.1.4](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.3...workspace-portal-v0.1.4) (2026-06-18)


### Features

* add --nosymfollow ([bff93ca](https://github.com/dstoc/workspace-portal/commit/bff93caa60c996478cb6405668901d698e06a6cb))
* disable hard links ([0bdf34d](https://github.com/dstoc/workspace-portal/commit/0bdf34db21ab4709433f6cf1ab3e3a9bb5466ee3))


### Bug Fixes

* ignore nosymfollow e2e when unsupported ([f904500](https://github.com/dstoc/workspace-portal/commit/f9045007edc6f304663ce13e1c502cc3f43b0f5d))
* skip nosymfollow FUSE e2e when mount stack rejects option ([13d09f8](https://github.com/dstoc/workspace-portal/commit/13d09f87a5ebd01c6e1260fb2de5de06716dd29e))


### Miscellaneous Chores

* release 0.1.4 ([77da55e](https://github.com/dstoc/workspace-portal/commit/77da55e146a18d29df11387e37cc477f87d9071b))

## [0.1.3](https://github.com/dstoc/workspace-portal/compare/workspace-portal-v0.1.2...workspace-portal-v0.1.3) (2026-06-17)


### Features

* Add diagnostic tracing for portal operations ([c3b139d](https://github.com/dstoc/workspace-portal/commit/c3b139dc344097f992e2a06d776787a733720439))


### Bug Fixes

* Allow no-op uid/gid setattr requests ([741f0c3](https://github.com/dstoc/workspace-portal/commit/741f0c3eff8bc4cd5dc0ed14a14a0ddd526d9008))

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
