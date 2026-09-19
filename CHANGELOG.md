# Changelog

## [0.2.4](https://github.com/perfectra1n/fjo/compare/v0.2.3...v0.2.4) (2026-09-19)


### Bug Fixes

* **dist:** run mise task bodies under bash, and dry-run the release path in CI ([436d213](https://github.com/perfectra1n/fjo/commit/436d21335b1a3911d61d4427e781c4f763b3e3a4))
* **dist:** run mise task bodies under bash, and dry-run the release path in CI ([24635e8](https://github.com/perfectra1n/fjo/commit/24635e884825b0f064aa938b226576544913c73a))

## [0.2.3](https://github.com/perfectra1n/fjo/compare/v0.2.2...v0.2.3) (2026-09-19)


### Features

* **dist:** ship musl artifacts, install scripts, and a GHCR image ([e80bb84](https://github.com/perfectra1n/fjo/commit/e80bb8434b377bb6966a8e6ae040c8348b690d09))
* **dist:** ship musl artifacts, install scripts, and a GHCR image ([743e9bd](https://github.com/perfectra1n/fjo/commit/743e9bd9066a0f86af57cd10b2579eb5f7da7964))


### Bug Fixes

* **core:** resolve the host from the checkout's remotes, not `active` ([ae6b71c](https://github.com/perfectra1n/fjo/commit/ae6b71ce22d2b61ec1096a58d0bc3a446b62367f))
* decide the host from the repository, not from `active` ([44ee20e](https://github.com/perfectra1n/fjo/commit/44ee20e86cec20a4cec9501dd3e25c92fef8562b))
* **fjo:** build the client for the host the repository resolved to ([32845ed](https://github.com/perfectra1n/fjo/commit/32845ede8a26c38874d231937468d1d90e092aaa))


### Documentation

* say which server a command talks to, and why ([a2c6c20](https://github.com/perfectra1n/fjo/commit/a2c6c20fce906fa81e710347a54faf1737ef3817))

## [0.2.2](https://github.com/perfectra1n/fjo/compare/v0.2.1...v0.2.2) (2026-09-19)


### Features

* **ci:** say whether spec drift is released or only on forgejo main ([048217b](https://github.com/perfectra1n/fjo/commit/048217b1301297efa37f1eb9af2c462da96874a9))
* **ci:** say whether spec drift is released or only on forgejo main ([10b1c43](https://github.com/perfectra1n/fjo/commit/10b1c43b7d5b9eaf53d6fb4a3464115a249a6400)), closes [#16](https://github.com/perfectra1n/fjo/issues/16)
* **config:** add the oauth_client_id preference ([51096f2](https://github.com/perfectra1n/fjo/commit/51096f27f3e0f26ad32565d159815fb14bac8a93))
* **core:** add Auth::Bearer for OAuth2 access tokens ([52c4d62](https://github.com/perfectra1n/fjo/commit/52c4d62a784e5fb26d3331890c5f52af41de5d3e))
* **core:** add URL-safe unpadded base64 ([5a21362](https://github.com/perfectra1n/fjo/commit/5a2136287646e8f4b3a6e564023f3d36e0162ff2))
* **core:** OAuth error taxonomy ([e7ce6a8](https://github.com/perfectra1n/fjo/commit/e7ce6a805cc6aec087d080c315c37e1e65ebddbd))
* **core:** OAuth2 PKCE, discovery and the token exchange ([92526a2](https://github.com/perfectra1n/fjo/commit/92526a267ca435fb50144f76357ebdc70ca47363))
* **core:** POST form requests on the instance web root ([fd63069](https://github.com/perfectra1n/fjo/commit/fd630699b997b8ba8a699605d74143dcad7165ba))
* **core:** redact OAuth codes and refresh tokens from traces ([98db522](https://github.com/perfectra1n/fjo/commit/98db522fe97caf4e285c2d91a8053cbc805e67ad))
* **core:** store OAuth2 credentials as one versioned document ([4598fdf](https://github.com/perfectra1n/fjo/commit/4598fdf68cda8c2e1e2e34a4e40c2b128bb730cf))
* **fjo:** fjo auth login --web ([fe27c1f](https://github.com/perfectra1n/fjo/commit/fe27c1fb53104c4441b680dd49435e94d63cb972))
* **fjo:** loopback listener for the OAuth redirect ([9c5d046](https://github.com/perfectra1n/fjo/commit/9c5d0465ef961a380fb49acffaa904fe36b064bc))
* **fjo:** refresh an expiring OAuth session before each command ([f8a721e](https://github.com/perfectra1n/fjo/commit/f8a721ebe29a6dbce4edea9a782f600909692483))
* **fjo:** report OAuth sessions in auth status and auth token ([25500e9](https://github.com/perfectra1n/fjo/commit/25500e973a5af28d9b4339e2d2072ca28e416162))
* log in through the browser with Forgejo's OAuth2 provider ([8a5883c](https://github.com/perfectra1n/fjo/commit/8a5883c2cbf858223b8ee88e5e9aca86ea0abfdd))
* **xtask:** let spec-diff compare any two refs, not just against vendored ([2268c3c](https://github.com/perfectra1n/fjo/commit/2268c3c12883775709770637aa641bda82771c71))


### Bug Fixes

* **coverage:** name the lock file plainly in the unknown-id error ([dab7ab0](https://github.com/perfectra1n/fjo/commit/dab7ab07bffbbef8fa3651efde1ce8dc1fef573d))
* **coverage:** start coverage-check from an empty journal directory ([62703fc](https://github.com/perfectra1n/fjo/commit/62703fc68b952204d69abd471eb58815ecadf448))
* **fjo:** refresh before handing git a credential ([b5fb772](https://github.com/perfectra1n/fjo/commit/b5fb772dbd7a0bdee4c9487cc3a5199c0c4a5c22))
* **itest:** bound the integration suite's wall clock ([92db776](https://github.com/perfectra1n/fjo/commit/92db7766311098df034c08de8085d3a6c22ba086))


### Code Refactoring

* **fjo:** open a URL without a Runtime ([d4944d6](https://github.com/perfectra1n/fjo/commit/d4944d63728d48d2c3bbd1965c203767d86c01b8))


### Documentation

* **coverage:** document the coverage ratchet and wire it into CI ([e0b7a2b](https://github.com/perfectra1n/fjo/commit/e0b7a2b0794de6fff57a3684dcd197c13f305f9c))
* **coverage:** replace the coverage prose with measured numbers ([c1c8460](https://github.com/perfectra1n/fjo/commit/c1c8460179bd328e80669881e00230692ef63407))
* unwrap the OAuth prose to match the rest ([af1b1ea](https://github.com/perfectra1n/fjo/commit/af1b1ea56330f20bc27582e1cce7a4727b63c48d))
* unwrap the prose and stop claiming the commands are untested ([9d13392](https://github.com/perfectra1n/fjo/commit/9d133922c714c4a3e0c1e6927a9d6b37796f216c))


### Tests

* **coverage:** check every generated request against its own metadata ([fad675c](https://github.com/perfectra1n/fjo/commit/fad675cfa3ad692456ac30b157cfb648d53b0228))
* **coverage:** record which commands the suites actually drove ([38acb1d](https://github.com/perfectra1n/fjo/commit/38acb1d0d354cbbec42233c31d81c2466e6aa36e))
* **itest:** drive every command group against a real Forgejo ([9cb1df0](https://github.com/perfectra1n/fjo/commit/9cb1df0f84780ae3ce312b7d5a505af20057b23b))
* **itest:** drive the local commands live and declare what the old suites drove ([0f137b4](https://github.com/perfectra1n/fjo/commit/0f137b459962c0da4799348904260d81af169135))
* **itest:** drive the whole OAuth login, consent click included ([cbff857](https://github.com/perfectra1n/fjo/commit/cbff857ada2824cabf2236fe16b043bd8f483700))
* **itest:** enable federation so the ActivityPub routes reach a handler ([11c35db](https://github.com/perfectra1n/fjo/commit/11c35dbd1549c33ea230c29abbbb05b11676b992))
* **itest:** give federation its own instance instead of the shared one ([40f4d10](https://github.com/perfectra1n/fjo/commit/40f4d1009962114a30ff824f7e6a7cb6555f9f16))
* **itest:** let a migration name this instance as its source ([028b6ff](https://github.com/perfectra1n/fjo/commit/028b6ff25c022f0e0bbd5d4dd84fbbf7e08ca903))
* **itest:** measure Forgejo's OAuth2 provider, and record what cannot be ([a4197bc](https://github.com/perfectra1n/fjo/commit/a4197bccf8ad2b23a55ea768a73a2e17241680ac))
* **itest:** stop the harness hiding forty-seven operations behind config ([cbe2134](https://github.com/perfectra1n/fjo/commit/cbe2134db70e8874a90f1258a18cc539c4932888))


### Miscellaneous Chores

* **spec:** bump vendored Forgejo spec to v16.0.5 (no API change) ([272f697](https://github.com/perfectra1n/fjo/commit/272f69743ac69b7bfc9bea06b77ee471c7d2eaed))
* **spec:** bump vendored Forgejo spec to v16.0.5 (no API change) ([90f7d9e](https://github.com/perfectra1n/fjo/commit/90f7d9eb9ec1494559f88e2facc281d6c1367c54))

## [0.2.1](https://github.com/perfectra1n/fjo/compare/v0.2.0...v0.2.1) (2026-09-16)


### Features

* **ci:** watch upstream Forgejo's spec and open bump PRs on drift ([bfb6a34](https://github.com/perfectra1n/fjo/commit/bfb6a3432a1fda5fa2f3fd92f4d3f5fcc711c896))
* **deps:** update rust (1.95.0 → 1.98.1) ([1ef2004](https://github.com/perfectra1n/fjo/commit/1ef200458aa47bc696e896964fd554c25ca44dbb))
* **xtask:** add spec-diff, a semantic report of upstream spec drift ([efb40de](https://github.com/perfectra1n/fjo/commit/efb40de329ce4666fb3f9694aad4689471e6666d))


### Bug Fixes

* **clippy:** drop redundant borrows flagged by rust 1.98 ([0705864](https://github.com/perfectra1n/fjo/commit/0705864ca00cf83e7bab69eb7b9b90e3a1042b06))
* **rust:** update crate clap (4.6.6 → 4.6.7) ([fdd3154](https://github.com/perfectra1n/fjo/commit/fdd3154749892de57aa7fe0b035e2068da3a8e74))
* **rust:** update crate clap_complete (4.6.9 → 4.6.11) ([d4e6dbc](https://github.com/perfectra1n/fjo/commit/d4e6dbca92c9cddd73856867e57edaeb1748c8c1))


### Documentation

* point toolchain comments at the 1.98.1 pin ([1fde351](https://github.com/perfectra1n/fjo/commit/1fde351ffdc7c51ab209fc090bc9355a5962043c))


### Miscellaneous Chores

* **github-action:** update github-actions ([c208eda](https://github.com/perfectra1n/fjo/commit/c208eda7d1af5f6e6385347daccd19296fff0b9a))
* land the open Renovate bumps and add upstream spec-drift automation ([ea200a0](https://github.com/perfectra1n/fjo/commit/ea200a0c1a23082c9151ee921955826a0657afa8))
* **rust:** lock file maintenance crate (cargo) ([b75c235](https://github.com/perfectra1n/fjo/commit/b75c2356d43dbd66124030bc66385c415dd9ffc1))

## [0.2.0](https://github.com/perfectra1n/fjo/compare/v0.1.0...v0.2.0) (2026-09-16)


### ⚠ BREAKING CHANGES

* four pieces of persisted user state move with no migration path, so an existing install will appear logged out and will lose its default-repo resolution until reconfigured.

### Features

* **api:** trim down API calls to Forgejo as much as possible ([06c2f08](https://github.com/perfectra1n/fjo/commit/06c2f0861f2df0a5974a741bcf46572af12564f1))
* **ci:** cut releases with release-please ([d881210](https://github.com/perfectra1n/fjo/commit/d8812106d4819a94f1e1ce9d9ad8d0883a0b0110))
* **license:** add license ([3f33bef](https://github.com/perfectra1n/fjo/commit/3f33bef74ee0eb3f76e82b5c5063eaad34e0a97e))
* **renovate:** implement renovate config ([60c41de](https://github.com/perfectra1n/fjo/commit/60c41dea26d103457b8b43b45bbcf693ee71ab9d))


### Bug Fixes

* **docstring:** update docstring to make cargo happy ([12fa9f1](https://github.com/perfectra1n/fjo/commit/12fa9f1dd9d68de344ec51785f11220cec5b179d))


### Code Refactoring

* rename fcli to fjo ([ef33082](https://github.com/perfectra1n/fjo/commit/ef33082e1eba55f9f2d9919509d9baa600348d9d))
