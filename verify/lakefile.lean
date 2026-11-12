import Lake
open Lake DSL

-- Veil models of Varaq's concurrency and recovery protocols. `lake build`
-- elaborates every model, which runs its `#model_check` commands; a violation
-- fails the build.

require veil from git "https://github.com/verse-lab/veil.git" @ "22cbf1affc1c4b6ab3fe290e773313dba7499165"

package varaqVerify

@[default_target]
lean_lib VaraqVerify {
  globs := #[.submodules `VaraqVerify]
}
