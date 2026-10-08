- **TESTING IS BEDROCK + T1** Injection rule: every component gets clock, network, disk,
  and randomness as inputs. Layers: (1) unit and property tests on every commit; (2)
  coverage-guided fuzzing of every decoder of outside input, short per merge and
  continuous nightly, crashes kept as regression inputs; (3) deterministic simulation of
  a whole mesh, thousands of runs per merge and millions nightly; (4) unit benchmarks
  and (5) component benchmarks on the dedicated machine with a 5% check; (6) end-to-end
  performance against P1 on shared infrastructure, nightly and per release; (7) a
  protocol simulator per connector on every merge; (8) Synnax HITL runners with real NI,
  LabJack, and PLC hardware, nightly and per release. Mutation testing runs on the diff
  (C9b).
