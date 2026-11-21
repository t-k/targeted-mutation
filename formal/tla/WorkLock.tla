----------------------------- MODULE WorkLock -----------------------------
(* A work directory has a stable lock file. Opening a legacy sentinel or
   creating the file refers to the same resource for all cooperating runs.
   Normal close and process death release the kernel's ownership. *)
EXTENDS FiniteSets

CONSTANT Runs
VARIABLES exists, owners, phase

vars == <<exists, owners, phase>>

Init ==
  /\ exists \in BOOLEAN
  /\ owners = {}
  /\ phase = [r \in Runs |-> "waiting"]

Acquire(r) ==
  /\ phase[r] = "waiting"
  /\ owners = {}
  /\ exists' = TRUE
  /\ owners' = owners \cup {r}
  /\ phase' = [phase EXCEPT ![r] = "working"]

Contend(r) ==
  /\ phase[r] = "waiting"
  /\ owners # {}
  /\ UNCHANGED vars

Release(r, reason) ==
  /\ r \in owners
  /\ owners' = owners \ {r}
  /\ phase' = [phase EXCEPT ![r] = reason]
  /\ UNCHANGED exists

Next ==
  \/ \E r \in Runs : Acquire(r) \/ Contend(r)
                      \/ Release(r, "finished") \/ Release(r, "killed")
  \/ /\ \A r \in Runs : phase[r] \in {"finished", "killed"}
     /\ UNCHANGED vars

Spec ==
  /\ Init /\ [][Next]_vars
  /\ WF_vars(\E r \in Runs : Acquire(r))
  /\ WF_vars(\E r \in Runs : Release(r, "finished") \/ Release(r, "killed"))

AtMostOneOwner == Cardinality(owners) \in {0, 1}
StableFile == owners # {} => exists
OnlyOwnersWork == owners = {r \in Runs : phase[r] = "working"}
FileNeverRemoved == [] (exists => [] exists)
CanReuseAfterRelease == <>(\A r \in Runs : phase[r] \in {"finished", "killed"})

=============================================================================
