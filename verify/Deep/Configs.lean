module

public import VaraqVerify.Latch
public import VaraqVerify.Mvcc

/-!
Larger instances of the models, too slow for every pull request. Each
`#mc_config` builds a model-checker binary under `.lake/model_checkers`;
`deep.sh` runs them. The scheduled CI job does both.
-/

#mc_config VaraqLatch { thread := Fin 3 } { maxWrites := 4 }
#mc_config VaraqMvcc { worker := Fin 2 } { maxClock := 5 } (maxDepth := 40)
