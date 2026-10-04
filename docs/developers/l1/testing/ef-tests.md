# Ethereum foundation tests

These are the official execution spec tests. There are two kinds: `state tests` and `blockchain tests`, you can execute them with:

### State tests

The state tests are individual transactions not related one to each other that test particular behavior of the EVM. Tests are usually run for multiple forks and the result of execution may vary between forks.
See [docs](https://steel.ethereum.foundation/docs/execution-specs/running_tests/test_formats/state_test/).

To run the test first:

```sh
cd tooling/ef_tests/state
```

then download the test vectors:

```sh
make download-evm-ef-tests
```

then run the tests:

```sh
make run-evm-ef-tests
```

### Blockchain tests


The blockchain tests test block validation and the consensus rules of the Ethereum blockchain. Tests are usually run for multiple forks.
See [docs](https://steel.ethereum.foundation/docs/execution-specs/running_tests/test_formats/blockchain_test/).

To run the tests first:

```sh
cd tooling/ef_tests/blockchain
```

then run the tests:

```sh
make test-levm
```

### Standalone runners

`blocktest` and `enginetest` run fixtures from any directory and print one result per fixture, which is how EELS `consume direct` calls them. From `tooling/`:

```sh
cargo run --profile release-fast -p ef_tests-runners --bin blocktest -- --path <fixtures> [--workers N] [--run REGEX] [--json] [--no-bal-parallel-exec] [--bal-report]
cargo run --profile release-fast -p ef_tests-runners --bin enginetest -- --path <fixtures> [same flags]
```

- `blocktest` imports each `blockchain_test` block through the blockchain ef_tests harness, handing it the fixture's `blockAccessList` (or `rlp_decoded.blockAccessList` for a block expected to be invalid) when the keccak of its RLP, in the order given, equals `blockAccessListHash`. A list that does not match or does not parse is dropped, as full sync drops a peer's, and the block runs without it.
- `enginetest` sends each `blockchain_test_engine` payload and forkchoice update to ethrex's own `engine_newPayloadV<n>` and `engine_forkchoiceUpdatedV<n>` handlers, in process.
- ethrex's `statetest` is `ef_tests-statev2 statetest` (see `tooling/ef_tests/state_v2`).
- `--version` prints one line naming the client and the tool, `ethrex-blocktest <version>`, `ethrex-enginetest <version>`, or `ethrex-statetest <version>` for `ef_tests-statev2 statetest --version`, where the version is the tooling workspace's; harnesses identify a runner by it.
- `--no-bal-parallel-exec` (or `ETHREX_NO_BAL_PARALLEL_EXEC`) runs every block on the sequential executor, like the node flag of the same name.
- With `--bal-report`, `blocktest` and `enginetest` print the executor chosen for each block as one JSON line on stderr, for example `{"event":"balExecution","block":1,"hash":"0x…","path":"sequential","reason":"disabled"}`. `reason` is `bad-access-list` when the runner dropped the block's delivered list, whatever ran the block. Otherwise it is empty for `parallel`, and for `sequential` the first condition that ruled parallel out: `no-access-list`, `pre-amsterdam`, `disabled` or `no-rayon`. A list over the EIP-7928 item cap shows as `no-access-list`, because the node drops it before the executor. Without the flag these lines are off. stdout carries only the results.
