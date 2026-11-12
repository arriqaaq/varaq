module

public import VaraqVerify.Latch
public import VaraqVerify.Mvcc

/-!
Larger instances of the models, too slow for every pull request. Each
`#mc_config` builds a model-checker binary under `.lake/model_checkers`;
`deep.sh` runs them. The scheduled CI job does both.

Each must fit in a CI runner's 16 GB. Measured on 4 cores:
* latch, 3 threads, 4 writes: 47.9M states, 7.1 GB, about 5 minutes;
* snapshots, 2 workers, 5 timestamps, depth 28: 16.1M states, 5.9 GB,
  about 2 minutes. Depth 30 needs 11.7 GB and depth 34 more than 14 GB;
  3 workers with 3 timestamps, or 4 latch threads, also run out.
-/

#mc_config VaraqLatch { thread := Fin 3 } { maxWrites := 4 }
#mc_config VaraqMvcc { worker := Fin 2 } { maxClock := 5 } (maxDepth := 28)
