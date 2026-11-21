----------------------------- MODULE WorkLock -----------------------------
(* A work directory has a stable lock file. Opening a legacy sentinel or
   creating the file refers to the same resource for all cooperating runs.
   The kernel releases ownership after the parent and any retained child
   have closed the descriptor. *)
EXTENDS FiniteSets

CONSTANT Runs
VARIABLES exists, owners, phase, children

vars == <<exists, owners, phase, children>>

Init ==
  /\ exists \in BOOLEAN
  /\ owners = {}
  /\ children = {}
  /\ phase = [r \in Runs |-> "waiting"]

Acquire(r) ==
  /\ phase[r] = "waiting"
  /\ owners = {}
  /\ exists' = TRUE
  /\ owners' = owners \cup {r}
  /\ phase' = [phase EXCEPT ![r] = "working"]
  /\ UNCHANGED children

Contend(r) ==
  /\ phase[r] = "waiting"
  /\ owners # {}
  /\ UNCHANGED vars

StartChild(r) ==
  /\ phase[r] = "working"
  /\ r \notin children
  /\ children' = children \cup {r}
  /\ UNCHANGED <<exists, owners, phase>>

ParentExit(r, reason) ==
  /\ phase[r] = "working"
  /\ owners' = IF r \in children THEN owners ELSE owners \ {r}
  /\ phase' = [phase EXCEPT ![r] = reason]
  /\ UNCHANGED <<exists, children>>

ChildExit(r) ==
  /\ r \in children
  /\ children' = children \ {r}
  /\ owners' = IF phase[r] = "working" THEN owners ELSE owners \ {r}
  /\ UNCHANGED <<exists, phase>>

Next ==
  \/ \E r \in Runs : Acquire(r) \/ Contend(r)
                      \/ StartChild(r) \/ ChildExit(r)
                      \/ ParentExit(r, "finished") \/ ParentExit(r, "killed")
  \/ /\ children = {}
     /\ \A r \in Runs : phase[r] \in {"finished", "killed"}
     /\ UNCHANGED vars

Spec ==
  /\ Init /\ [][Next]_vars
  /\ WF_vars(\E r \in Runs : Acquire(r))
  /\ WF_vars(\E r \in Runs : ParentExit(r, "finished") \/ ParentExit(r, "killed"))
  /\ WF_vars(\E r \in Runs : ChildExit(r))

AtMostOneOwner == Cardinality(owners) \in {0, 1}
StableFile == owners # {} => exists
OnlyOwnersWork == owners = {r \in Runs : phase[r] = "working" \/ r \in children}
FileNeverRemoved == [] (exists => [] exists)
CanReuseAfterRelease == <>(children = {} /\ \A r \in Runs : phase[r] \in {"finished", "killed"})

=============================================================================
