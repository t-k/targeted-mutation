--------------------------- MODULE TargetCoverage ---------------------------
(* Each candidate represents one source edit. The baseline's Cargo artifacts
   establish whether its file was compiled. A finished edit is restored before
   its result is recorded. *)
EXTENDS FiniteSets, Naturals

CONSTANTS Roots, Candidates
VARIABLES owners, built, compiled, remaining, reported, file, phase, edited, outcome

vars == <<owners, built, compiled, remaining, reported, file, phase, edited, outcome>>
Verdicts == {"killed", "survived", "timeout", "unviable", "not-evaluated"}

Init ==
  /\ owners \in [Candidates -> SUBSET Roots]
  /\ \A f \in Candidates : owners[f] # {}
  /\ built \in SUBSET Roots
  /\ compiled \in SUBSET {f \in Candidates : owners[f] \cap built # {}}
  /\ remaining = Candidates
  /\ reported = {}
  /\ file = CHOOSE f \in Candidates : TRUE
  /\ phase = "idle"
  /\ edited = {}
  /\ outcome = "pending"

Select ==
  /\ phase = "idle"
  /\ file' \in remaining
  /\ IF file' \notin compiled
        THEN /\ phase' = "restore"
             /\ edited' = {}
             /\ outcome' = "not-evaluated"
        ELSE /\ phase' = "build"
             /\ edited' = {file'}
             /\ outcome' = "pending"
  /\ UNCHANGED <<owners, built, compiled, remaining, reported>>

Build ==
  /\ phase = "build"
  /\ \/ /\ phase' = "test"
        /\ outcome' = "pending"
     \/ /\ phase' = "restore"
        /\ outcome' = "unviable"
  /\ UNCHANGED <<owners, built, compiled, remaining, reported, file, edited>>

Test ==
  /\ phase = "test"
  /\ outcome' \in {"killed", "survived", "timeout", "not-evaluated"}
  /\ phase' = "restore"
  /\ UNCHANGED <<owners, built, compiled, remaining, reported, file, edited>>

Restore ==
  /\ phase = "restore"
  /\ edited' = {}
  /\ phase' = "report"
  /\ UNCHANGED <<owners, built, compiled, remaining, reported, file, outcome>>

Report ==
  /\ phase = "report"
  /\ reported' = reported \cup {<<file, outcome>>}
  /\ remaining' = remaining \ {file}
  /\ phase' = "idle"
  /\ outcome' = "pending"
  /\ UNCHANGED <<owners, built, compiled, file, edited>>

Done ==
  /\ phase = "idle"
  /\ remaining = {}
  /\ UNCHANGED vars

Next == Select \/ Build \/ Test \/ Restore \/ Report \/ Done
Spec == Init /\ [][Next]_vars
        /\ WF_vars(Select) /\ WF_vars(Build) /\ WF_vars(Test)
        /\ WF_vars(Restore) /\ WF_vars(Report)

Restored ==
  phase \in {"idle", "report"} => edited = {}

OneReportPerCandidate ==
  /\ \A f \in Candidates : Cardinality({r \in reported : r[1] = f}) <= 1
  /\ remaining \cup {r[1] : r \in reported} = Candidates
  /\ remaining \cap {r[1] : r \in reported} = {}

SupportedVerdict ==
  \A r \in reported :
    /\ r[2] \in Verdicts
    /\ r[1] \notin compiled => r[2] = "not-evaluated"

Completes == <>(remaining = {} /\ edited = {})
=============================================================================
