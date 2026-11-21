----------------------------- MODULE WorkLock -----------------------------
(* The legacy protocol reserves a file. The current protocol atomically
   reserves a directory at the same path. Its kernel lock remains held
   until the parent and any retained child have closed their descriptors. *)
EXTENDS FiniteSets

CONSTANT Runs
VARIABLES marker, legacyPhase, owners, phase, children

vars == <<marker, legacyPhase, owners, phase, children>>

Init ==
  /\ marker \in {"absent", "legacy", "current"}
  /\ legacyPhase \in IF marker = "legacy" THEN {"running", "killed"}
                        ELSE {"waiting"}
  /\ owners = {}
  /\ children = {}
  /\ phase = [r \in Runs |-> "waiting"]

LegacyAcquire ==
  /\ marker = "absent"
  /\ legacyPhase = "waiting"
  /\ marker' = "legacy"
  /\ legacyPhase' = "running"
  /\ UNCHANGED <<owners, phase, children>>

LegacyExit ==
  /\ legacyPhase = "running"
  /\ \/ /\ legacyPhase' = "finished"
        /\ marker' = "absent"
     \/ /\ legacyPhase' = "killed"
        /\ UNCHANGED marker
  /\ UNCHANGED <<owners, phase, children>>

Migrate ==
  /\ marker = "legacy"
  /\ legacyPhase # "running"
  /\ marker' = "absent"
  /\ UNCHANGED <<legacyPhase, owners, phase, children>>

Initialize ==
  /\ marker = "absent"
  /\ marker' = "current"
  /\ UNCHANGED <<legacyPhase, owners, phase, children>>

Acquire(r) ==
  /\ marker = "current"
  /\ phase[r] = "waiting"
  /\ owners = {}
  /\ owners' = owners \cup {r}
  /\ phase' = [phase EXCEPT ![r] = "working"]
  /\ UNCHANGED <<marker, legacyPhase, children>>

Contend(r) ==
  /\ phase[r] = "waiting"
  /\ marker = "legacy" \/ owners # {}
  /\ UNCHANGED vars

StartChild(r) ==
  /\ phase[r] = "working"
  /\ r \notin children
  /\ children' = children \cup {r}
  /\ UNCHANGED <<marker, legacyPhase, owners, phase>>

ParentExit(r, reason) ==
  /\ phase[r] = "working"
  /\ owners' = IF r \in children THEN owners ELSE owners \ {r}
  /\ phase' = [phase EXCEPT ![r] = reason]
  /\ UNCHANGED <<marker, legacyPhase, children>>

ChildExit(r) ==
  /\ r \in children
  /\ children' = children \ {r}
  /\ owners' = IF phase[r] = "working" THEN owners ELSE owners \ {r}
  /\ UNCHANGED <<marker, legacyPhase, phase>>

AllStopped ==
  /\ children = {}
  /\ \A r \in Runs : phase[r] \in {"finished", "killed"}

Next ==
  \/ LegacyAcquire \/ LegacyExit \/ Migrate \/ Initialize
  \/ \E r \in Runs : Acquire(r) \/ Contend(r)
                      \/ StartChild(r) \/ ChildExit(r)
                      \/ ParentExit(r, "finished") \/ ParentExit(r, "killed")
  \/ AllStopped /\ UNCHANGED vars

Spec ==
  /\ Init /\ [][Next]_vars
  /\ WF_vars(LegacyExit)
  /\ WF_vars(Migrate)
  /\ WF_vars(Initialize)
  /\ WF_vars(\E r \in Runs : Acquire(r))
  /\ WF_vars(\E r \in Runs : ParentExit(r, "finished") \/ ParentExit(r, "killed"))
  /\ WF_vars(\E r \in Runs : ChildExit(r))

AtMostOneOwner == Cardinality(owners) \in {0, 1}
StableDirectory == owners # {} => marker = "current"
ProtocolsExclude == legacyPhase = "running" => marker = "legacy" /\ owners = {}
OnlyOwnersWork == owners = {r \in Runs : phase[r] = "working" \/ r \in children}
DirectoryNeverRemoved == [] (marker = "current" => [] (marker = "current"))
CanReuseAfterRelease == <>AllStopped

=============================================================================
