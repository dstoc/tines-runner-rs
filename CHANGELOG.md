# Changelog

## [0.8.1](https://github.com/dstoc/tines-runner-rs/compare/v0.8.0...v0.8.1) (2026-10-08)


### Bug Fixes

* **executor:** allow slower capability transports ([#79](https://github.com/dstoc/tines-runner-rs/issues/79)) ([14e8f26](https://github.com/dstoc/tines-runner-rs/commit/14e8f26ffdb2f444c7589aba46c673545afa623a))

## [0.8.0](https://github.com/dstoc/tines-runner-rs/compare/v0.7.0...v0.8.0) (2026-10-08)


### Features

* **antigravity:** classify structured provider errors ([#74](https://github.com/dstoc/tines-runner-rs/issues/74)) ([844eef4](https://github.com/dstoc/tines-runner-rs/commit/844eef4a8efb101bb3e267adc6f0de068cb02e9f))
* **harness:** add first-class Antigravity support ([#72](https://github.com/dstoc/tines-runner-rs/issues/72)) ([7d446d9](https://github.com/dstoc/tines-runner-rs/commit/7d446d97d40cdfb0d734a5953053ae25b34db458))


### Bug Fixes

* **antigravity:** harden capability discovery ([#73](https://github.com/dstoc/tines-runner-rs/issues/73)) ([3a06fe4](https://github.com/dstoc/tines-runner-rs/commit/3a06fe41ace1ff8960246e3714986678ab004ccd))
* **config:** resolve daemon paths relative to config directory ([#70](https://github.com/dstoc/tines-runner-rs/issues/70)) ([ce748c2](https://github.com/dstoc/tines-runner-rs/commit/ce748c20650a1fb8ad10f9d95ff36dab84d567bc))
* **executor:** fail on harness stdin write errors ([#77](https://github.com/dstoc/tines-runner-rs/issues/77)) ([791af37](https://github.com/dstoc/tines-runner-rs/commit/791af37a337bf867a3f27dcd380fae532f26ba26))
* **executor:** scope capability health to configured harness ([#75](https://github.com/dstoc/tines-runner-rs/issues/75)) ([ed9a96b](https://github.com/dstoc/tines-runner-rs/commit/ed9a96b3b7958a6165a98eccd216a348df8e9e75))

## [0.7.0](https://github.com/dstoc/tines-runner-rs/compare/v0.6.0...v0.7.0) (2026-10-07)


### Features

* **runner:** supervise all configured runners ([#68](https://github.com/dstoc/tines-runner-rs/issues/68)) ([573a778](https://github.com/dstoc/tines-runner-rs/commit/573a7786e559ec939d7ce84baa892e9b05b8449b))


### Bug Fixes

* **poll:** omit effort capabilities for custom runners ([#67](https://github.com/dstoc/tines-runner-rs/issues/67)) ([2561964](https://github.com/dstoc/tines-runner-rs/commit/2561964b91299ca45c0998ba8ca73e32eb132775))

## [0.6.0](https://github.com/dstoc/tines-runner-rs/compare/v0.5.0...v0.6.0) (2026-10-06)


### Features

* **config:** add runner default inheritance ([#66](https://github.com/dstoc/tines-runner-rs/issues/66)) ([7921fdc](https://github.com/dstoc/tines-runner-rs/commit/7921fdc7ce23d3341a76e25a04d86dd074831bf6))
* **recovery:** separate runner state from credentials ([#65](https://github.com/dstoc/tines-runner-rs/issues/65)) ([08ed574](https://github.com/dstoc/tines-runner-rs/commit/08ed5741eb5d8e975417ef9aa723591cbc937a21))


### Bug Fixes

* **runner:** include custom command in registration ([#63](https://github.com/dstoc/tines-runner-rs/issues/63)) ([0fe73da](https://github.com/dstoc/tines-runner-rs/commit/0fe73dad5f0ca1af27ea7f37875b6ac74bf9c33b))

## [0.5.0](https://github.com/dstoc/tines-runner-rs/compare/v0.4.1...v0.5.0) (2026-10-06)


### Features

* **config:** require explicit --config path ([#59](https://github.com/dstoc/tines-runner-rs/issues/59)) ([8b5e4ae](https://github.com/dstoc/tines-runner-rs/commit/8b5e4ae4548d0b0e14750f8b00556c9522127314))
* **config:** support multiple named runners ([#62](https://github.com/dstoc/tines-runner-rs/issues/62)) ([23a5e09](https://github.com/dstoc/tines-runner-rs/commit/23a5e094cfe046de088e1d5be829930c4ff66df7))
* **credentials:** add explicit runner registration command ([#61](https://github.com/dstoc/tines-runner-rs/issues/61)) ([aea8712](https://github.com/dstoc/tines-runner-rs/commit/aea871243b18c2af41d9f7a0a67f96ce961fb617))
* **logging:** improve daemon operational diagnostics ([#60](https://github.com/dstoc/tines-runner-rs/issues/60)) ([a60b632](https://github.com/dstoc/tines-runner-rs/commit/a60b6323b9d693411904649f2e524432b2f48518))


### Bug Fixes

* **credentials:** allow read-only external credential files ([#57](https://github.com/dstoc/tines-runner-rs/issues/57)) ([ee9bb78](https://github.com/dstoc/tines-runner-rs/commit/ee9bb784e41cfac31b90e925fd028b2c64fe9f0b))

## [0.4.1](https://github.com/dstoc/tines-runner-rs/compare/v0.4.0...v0.4.1) (2026-10-06)


### Bug Fixes

* **executor:** restore Codex pricing evidence ([#55](https://github.com/dstoc/tines-runner-rs/issues/55)) ([7267232](https://github.com/dstoc/tines-runner-rs/commit/7267232945cdd1ea6da244ebc8c656f2330cbc71))
* reduce runner log noise ([#53](https://github.com/dstoc/tines-runner-rs/issues/53)) ([90d8910](https://github.com/dstoc/tines-runner-rs/commit/90d8910b0e6dba4e76d8ffa05eff7bf9ea01f69a))
* scope effort validation to resolved harness ([#56](https://github.com/dstoc/tines-runner-rs/issues/56)) ([6abc42a](https://github.com/dstoc/tines-runner-rs/commit/6abc42aebbe737958e3bb575214b082cc9306e11))

## [0.4.0](https://github.com/dstoc/tines-runner-rs/compare/v0.3.0...v0.4.0) (2026-10-05)


### Features

* **config:** add capabilities_executor for capability discovery ([#51](https://github.com/dstoc/tines-runner-rs/issues/51)) ([09e77d3](https://github.com/dstoc/tines-runner-rs/commit/09e77d3f11db02dbfe6c3529f6a2e388a1cf8c0d))

## [0.3.0](https://github.com/dstoc/tines-runner-rs/compare/v0.2.0...v0.3.0) (2026-10-05)


### Features

* **security:** add configurable run key delivery ([#49](https://github.com/dstoc/tines-runner-rs/issues/49)) ([6d08003](https://github.com/dstoc/tines-runner-rs/commit/6d080032cb0d777ef47bdc626d86044f1a00839f))

## [0.2.0](https://github.com/dstoc/tines-runner-rs/compare/v0.1.0...v0.2.0) (2026-10-04)


### Features

* add daemon executor transport ([#34](https://github.com/dstoc/tines-runner-rs/issues/34)) ([a58f783](https://github.com/dstoc/tines-runner-rs/commit/a58f783edeb6accb6a337f9588e61dc1949c11f7))
* add executor harness adapters ([#36](https://github.com/dstoc/tines-runner-rs/issues/36)) ([4d5b0d9](https://github.com/dstoc/tines-runner-rs/commit/4d5b0d991ced566e47354bfe53baa791e81b0417))
* **cli:** support selecting config file ([#29](https://github.com/dstoc/tines-runner-rs/issues/29)) ([3df8a95](https://github.com/dstoc/tines-runner-rs/commit/3df8a953d3354dca3b03bcc8f69e315cc4e03a9e))
* **config:** require daemon executor working directory ([#38](https://github.com/dstoc/tines-runner-rs/issues/38)) ([a726b28](https://github.com/dstoc/tines-runner-rs/commit/a726b28e89d7bcb93cc431c6038323ca032de3b2))
* **execution:** report generic executor events ([#40](https://github.com/dstoc/tines-runner-rs/issues/40)) ([fb79c64](https://github.com/dstoc/tines-runner-rs/commit/fb79c64d41fda22408a615d3ded8a1f8d63910b2))
* **executor:** add one-shot execute CLI ([#33](https://github.com/dstoc/tines-runner-rs/issues/33)) ([2172fb7](https://github.com/dstoc/tines-runner-rs/commit/2172fb7b05269b45b3713e059d7483b995cab621))
* **executor:** discover capabilities through configured transport ([#39](https://github.com/dstoc/tines-runner-rs/issues/39)) ([590f49f](https://github.com/dstoc/tines-runner-rs/commit/590f49f9be2e2638c7d9ec7ce8118d1e442145fc))
* **executor:** prepare workspaces from requests ([#35](https://github.com/dstoc/tines-runner-rs/issues/35)) ([c4e5bff](https://github.com/dstoc/tines-runner-rs/commit/c4e5bff792ca1d7e2e674564e21e5953ededc6c6))
* **executor:** supervise harness lifecycle ([#37](https://github.com/dstoc/tines-runner-rs/issues/37)) ([c29e059](https://github.com/dstoc/tines-runner-rs/commit/c29e059d9f08855cd41b2ba72b4abb56cf011800))
* **harness:** add configurable custom command support ([#46](https://github.com/dstoc/tines-runner-rs/issues/46)) ([ec663ec](https://github.com/dstoc/tines-runner-rs/commit/ec663ec7eab6400164e3a15ef85625063396b4e8))
* **protocol:** define local execution protocol v1 ([#32](https://github.com/dstoc/tines-runner-rs/issues/32)) ([04edc3c](https://github.com/dstoc/tines-runner-rs/commit/04edc3c94dbaa4870bcd87700483e3dc57fa4659))
* **supervision:** track executor lifecycle state ([#42](https://github.com/dstoc/tines-runner-rs/issues/42)) ([37aa4bf](https://github.com/dstoc/tines-runner-rs/commit/37aa4bf80a2d7a70a85be3f25649a02bea0c4dff))
* **workspace:** add repository checkout policy ([#47](https://github.com/dstoc/tines-runner-rs/issues/47)) ([2a62f4a](https://github.com/dstoc/tines-runner-rs/commit/2a62f4ab973a5a86b08033ca0170c686426bd37a))

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
