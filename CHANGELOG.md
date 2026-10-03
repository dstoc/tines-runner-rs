# Changelog

## 0.1.0 (2026-10-03)


### Features

* add Tines runner protocol client ([#5](https://github.com/dstoc/tines-runner-rs/issues/5)) ([f4fb6d5](https://github.com/dstoc/tines-runner-rs/commit/f4fb6d504237e756db43a4a7cac3b17f16c48f06))
* **codex:** advertise and enforce effort capabilities ([#14](https://github.com/dstoc/tines-runner-rs/issues/14)) ([a121713](https://github.com/dstoc/tines-runner-rs/commit/a121713f004737a225912bef549ddeb82964b8a5))
* **codex:** build structured invocation and wrapper command ([#10](https://github.com/dstoc/tines-runner-rs/issues/10)) ([07da684](https://github.com/dstoc/tines-runner-rs/commit/07da684c721e0d3983e4bf5e63c569e06f51f743))
* **codex:** parse and render JSONL streams ([#12](https://github.com/dstoc/tines-runner-rs/issues/12)) ([451f1eb](https://github.com/dstoc/tines-runner-rs/commit/451f1eb920f2e7e4077dad32335f0c7fd09c87ac))
* **config:** add TOML config and override resolution ([#3](https://github.com/dstoc/tines-runner-rs/issues/3)) ([17ae9d7](https://github.com/dstoc/tines-runner-rs/commit/17ae9d7e89af7887664feeb4d0996e1473362af0))
* create Rust project skeleton ([#2](https://github.com/dstoc/tines-runner-rs/issues/2)) ([fdcf8ce](https://github.com/dstoc/tines-runner-rs/commit/fdcf8ce1134da4e8bf6dd212df60f33fe53dacbe))
* **finish:** report outcomes and Codex usage ([#16](https://github.com/dstoc/tines-runner-rs/issues/16)) ([a3c9047](https://github.com/dstoc/tines-runner-rs/commit/a3c90473151e007e4f3244fcc34da7840fd60cd7))
* implement graceful shutdown and draining ([#20](https://github.com/dstoc/tines-runner-rs/issues/20)) ([42adbdd](https://github.com/dstoc/tines-runner-rs/commit/42adbdd6440a252ccbeed9f594f486de719b24fd))
* **logs:** batch and retry sequenced run logs ([#17](https://github.com/dstoc/tines-runner-rs/issues/17)) ([bd7d752](https://github.com/dstoc/tines-runner-rs/commit/bd7d752efc1eae920a2ff22c29f655cb4059ad06))
* **process:** supervise Codex process groups ([#13](https://github.com/dstoc/tines-runner-rs/issues/13)) ([3893673](https://github.com/dstoc/tines-runner-rs/commit/3893673ce1f3d8687802bdb874f7684916574467))
* **recovery:** persist active runs and terminate orphans ([#19](https://github.com/dstoc/tines-runner-rs/issues/19)) ([052b093](https://github.com/dstoc/tines-runner-rs/commit/052b093e498b4934c7c2f6288b20dbdb134dc63e))
* **release:** automate versioned Linux releases ([#27](https://github.com/dstoc/tines-runner-rs/issues/27)) ([c414d31](https://github.com/dstoc/tines-runner-rs/commit/c414d316f38464641889c15a0fcb0b8542887fcb))
* **runner:** add daemon poll loop and fencing ([#7](https://github.com/dstoc/tines-runner-rs/issues/7)) ([1806595](https://github.com/dstoc/tines-runner-rs/commit/1806595c2f3c7172b8fa8fe36ee9858fb0490efa))
* **runner:** detect Codex provider rate limits ([#22](https://github.com/dstoc/tines-runner-rs/issues/22)) ([2377d7d](https://github.com/dstoc/tines-runner-rs/commit/2377d7dcb0ad7765888eefe9307103deaf782f5c))
* **runner:** handle assignment timeout and cancellation ([#21](https://github.com/dstoc/tines-runner-rs/issues/21)) ([53cb570](https://github.com/dstoc/tines-runner-rs/commit/53cb570b8c140b2cd498f2560f537f83ba7bdc6d))
* **runner:** reconcile concurrent assignments ([#15](https://github.com/dstoc/tines-runner-rs/issues/15)) ([e82f618](https://github.com/dstoc/tines-runner-rs/commit/e82f618bab89093ca9e821cdd7994dd5c916428a))
* **runner:** register and reconnect local runners ([#6](https://github.com/dstoc/tines-runner-rs/issues/6)) ([0b56581](https://github.com/dstoc/tines-runner-rs/commit/0b565813a4e5892902fdc3dd5024257e42a4ca3a))
* **runner:** resolve assignment-specific config ([#8](https://github.com/dstoc/tines-runner-rs/issues/8)) ([9969602](https://github.com/dstoc/tines-runner-rs/commit/9969602367f7276835cfcabcd864681359cca47e))
* store runner credentials securely ([#4](https://github.com/dstoc/tines-runner-rs/issues/4)) ([47fc025](https://github.com/dstoc/tines-runner-rs/commit/47fc0258b315c14e620432c07b47c0be670f9470))
* **workspace:** clone effective repositories ([#11](https://github.com/dstoc/tines-runner-rs/issues/11)) ([e31f74d](https://github.com/dstoc/tines-runner-rs/commit/e31f74dbf31c1ce44f92ba7bde319b29014b1210))
* **workspace:** materialize cold-run assignment workspaces ([#9](https://github.com/dstoc/tines-runner-rs/issues/9)) ([bc81445](https://github.com/dstoc/tines-runner-rs/commit/bc81445197831a1d78256f296c846fe282d195f6))
* **workspace:** retain and prune settled workspaces ([#18](https://github.com/dstoc/tines-runner-rs/issues/18)) ([7b84c93](https://github.com/dstoc/tines-runner-rs/commit/7b84c935808c4ea1545aacd1836432956df668d3))
