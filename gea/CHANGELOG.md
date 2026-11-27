# Changelog

## [0.2.2](https://github.com/perfectra1n/gea/compare/v0.2.1...v0.2.2) (2026-11-10)


### Bug Fixes

* **codegen:** generate untyped properties as free-form JSON ([91469e1](https://github.com/perfectra1n/gea/commit/91469e126509ab8e47cd08a00597e8031b89ed17))


### Tests

* **itest:** compare the server version with the pinned image ([23eaced](https://github.com/perfectra1n/gea/commit/23eaced662592fd83e07a10df4adff24096f5d35))

## [0.2.1](https://github.com/perfectra1n/gea/compare/v0.2.0...v0.2.1) (2026-11-09)


### Features

* **pr:** inline review threads, replies, resolution and line comments ([43571d7](https://github.com/perfectra1n/gea/commit/43571d78e5b8bb54df49bffe8541b1a0a690ba47))
* **pr:** inline review threads, replies, resolution and line comments ([869cb7d](https://github.com/perfectra1n/gea/commit/869cb7d347518a6884d0b0ba3ee0b10c1a0886fc))
* **spec:** update Gitea API spec to v28.2.0 ([aeabbbf](https://github.com/perfectra1n/gea/commit/aeabbbf1567c4a1a5b899e44a51b72e43396babd))
* **spec:** update Gitea API spec to v28.3.0 ([b7e532a](https://github.com/perfectra1n/gea/commit/b7e532a7a5e1435f83eff5df60cae7edb238b131))


### Bug Fixes

* **deps:** clear this week's cargo-deny findings ([7330224](https://github.com/perfectra1n/gea/commit/7330224d0495ee2e64d2a2da68ae0bb6bb68ef79))
* **deps:** clear this week's RustSec advisories ([7667a96](https://github.com/perfectra1n/gea/commit/7667a964c733c5b36d5e63c31f3945e80673d0cd))


### Code Refactoring

* **pr:** take the pull request as the positional on reply, resolve and unresolve ([b2e7c4e](https://github.com/perfectra1n/gea/commit/b2e7c4e2814b6125c068f6798b2816b9a17a8630))


### Miscellaneous Chores

* **spec:** verify the loader counts against Gitea v28.3.0 ([85c0284](https://github.com/perfectra1n/gea/commit/85c0284e8bd0e6e2b195aafc1ee24ac88ffdaa0b))

## [0.2.0](https://github.com/perfectra1n/gea/compare/v0.1.2...v0.2.0) (2026-10-12)


### ⚠ BREAKING CHANGES

* **spec:** gitea-model drops the NodeInfo* types, which Gitea 28 removed; `ValueTy` gains a variant; gea now speaks the Gitea 28 API.

### Features

* **project:** drive boards through Gitea 28's REST projects API ([df7fe28](https://github.com/perfectra1n/gea/commit/df7fe2842318e721ae1e6798bd9e3f84f4f533ce))
* **project:** drive boards through Gitea 28's REST projects API ([15f8f60](https://github.com/perfectra1n/gea/commit/15f8f606965d139d1e9bc8ff3ea304acdf0df1a1))
* **spec:** target Gitea 28.1.0 ([fc1c4cb](https://github.com/perfectra1n/gea/commit/fc1c4cbf95c770c7dddbdf7233578f208d70bb6e))


### Bug Fixes

* **release:** make --version report the release version ([b05ce10](https://github.com/perfectra1n/gea/commit/b05ce101d6eb32c835b1023cfd643e70ba7f0cea))
* **release:** make --version report the release version ([03131db](https://github.com/perfectra1n/gea/commit/03131db3e4a6e94475934c501e253b77d1d49e30))


### Tests

* **itest:** run the avatar test as an account of its own ([3607d04](https://github.com/perfectra1n/gea/commit/3607d04a3ed6fb92747f6b41d56e3f599df0b13a))
* **itest:** run the avatar test as an account of its own ([392ea0d](https://github.com/perfectra1n/gea/commit/392ea0d6014d248b9451123efcde8dc85f1f9b25))

## [0.1.2](https://github.com/perfectra1n/gea/compare/v0.1.1...v0.1.2) (2026-10-10)


### Features

* **ci:** core run/job/target resolution, CiFailed/CiPending, and a real runner for live tests ([61de034](https://github.com/perfectra1n/gea/commit/61de034adad226c9396298becc8c6b4a07cc9a07))
* **ci:** gea ci group, run/job refs across gea run, pr checks summary + next hints ([ba40b50](https://github.com/perfectra1n/gea/commit/ba40b505f14766ffad4cf2c978cc45c1a8770b53))


### Bug Fixes

* **ci:** address code review on the CI porcelain ([b5532bc](https://github.com/perfectra1n/gea/commit/b5532bcdae9ed0b79749b2189a9ae9ef136a5a4d))
* **pr:** anchor `pr create --fill` to the server's base commit ([40d7de8](https://github.com/perfectra1n/gea/commit/40d7de893331e95bda5a9ad8645395cb802b8e3e))
* **pr:** anchor `pr create --fill` to the server's base commit ([ebc4a96](https://github.com/perfectra1n/gea/commit/ebc4a96d39c56e3177ac282953a38d3fbdd9e16d)), closes [#2](https://github.com/perfectra1n/gea/issues/2)
* **xtask:** read Gitea 28's relocated swagger spec ([44ec066](https://github.com/perfectra1n/gea/commit/44ec066ef9264d6dda5d4c353f1e09fd2430e371))
* **xtask:** read Gitea 28's relocated swagger spec (spec-drift 404) ([afa6bf4](https://github.com/perfectra1n/gea/commit/afa6bf461a60faaeb8ae88680306377b82326cb6))


### Documentation

* **spec:** CI porcelain design — gea ci, flexible run/job refs, pr checks hints ([66b7182](https://github.com/perfectra1n/gea/commit/66b71829542aaa320e1b2bbdb2dba4196ea67079))


### Tests

* **ci:** live end-to-end gea ci/run/pr checks tests against a real runner ([5de31f2](https://github.com/perfectra1n/gea/commit/5de31f2d3df7c19373fcee78dad6386f33438fef))


### Continuous Integration

* **budget:** check the release profile directly and size the budget for step changes ([166b888](https://github.com/perfectra1n/gea/commit/166b888d33bddd19b2f73992b4810160def83426))


### Miscellaneous Chores

* **deps:** bump yanked yoke-derive 0.8.3 to 0.8.4 ([e2a679e](https://github.com/perfectra1n/gea/commit/e2a679eec32f2e1c23c316c7e846ae38cc541716))

## [0.1.1](https://github.com/perfectra1n/gea/compare/v0.1.0...v0.1.1) (2026-09-24)


### Features

* port fjo to Gitea 1.27.3 as gea ([3409f3e](https://github.com/perfectra1n/gea/commit/3409f3e5f5d563ef6819885acf8efa21328dd1a5))

## Changelog
