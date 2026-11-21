-------------------------- MODULE OutputFixture --------------------------
(* The work-directory lock serializes staging and builds. Paths and permissions
   are validated before the journal changes. Each path here is a regular file.
   Directory leaves are removed separately and only when empty. *)
EXTENDS FiniteSets

CONSTANT Files

VARIABLES disk, receipts, desired, remaining, phase, external

vars == <<disk, receipts, desired, remaining, phase, external>>
Copies == {"copyA", "copyB"}
Absent == "absent"
Cargo == "external"
Items == Copies \cup {Absent, Cargo}

Init ==
  /\ disk = [f \in Files |-> Absent]
  /\ receipts = [f \in Files |-> {}]
  /\ desired = [f \in Files |-> Absent]
  /\ remaining = {}
  /\ phase = "idle"
  /\ external = {}

NewExternal ==
  /\ phase = "idle"
  /\ \E f \in Files \ external:
     /\ disk' = [disk EXCEPT ![f] = Cargo]
     /\ external' = external \cup {f}
  /\ UNCHANGED <<receipts, desired, remaining, phase>>

Plan ==
  /\ phase = "idle"
  /\ desired' \in [Files -> Copies \cup {Absent}]
  /\ \A f \in external : desired'[f] = Absent
  /\ remaining' = Files
  /\ phase' = "planned"
  /\ UNCHANGED <<disk, receipts, external>>

Journal ==
  /\ phase = "planned"
  /\ receipts' = [f \in Files |->
       (IF disk[f] \in receipts[f] THEN {disk[f]} ELSE {})
       \cup (IF desired[f] \in Copies THEN {desired[f]} ELSE {})]
  /\ phase' = "applying"
  /\ UNCHANGED <<disk, desired, remaining, external>>

Apply ==
  /\ phase = "applying"
  /\ \E f \in remaining:
     /\ disk' = IF desired[f] \in Copies \/ disk[f] \in receipts[f]
                THEN [disk EXCEPT ![f] = desired[f]]
                ELSE disk
     /\ remaining' = remaining \ {f}
  /\ UNCHANGED <<receipts, desired, external, phase>>

Commit ==
  /\ phase = "applying"
  /\ remaining = {}
  /\ receipts' = [f \in Files |-> IF desired[f] \in Copies THEN {desired[f]} ELSE {}]
  /\ phase' = "complete"
  /\ UNCHANGED <<disk, desired, remaining, external>>

Finish ==
  /\ phase = "complete"
  /\ phase' = "idle"
  /\ UNCHANGED <<disk, receipts, desired, remaining, external>>

Interrupt ==
  /\ phase \in {"planned", "applying"}
  /\ phase' = "idle"
  /\ UNCHANGED <<disk, receipts, desired, remaining, external>>

Next == NewExternal \/ Plan \/ Journal \/ Apply \/ Commit \/ Finish \/ Interrupt
Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ disk \in [Files -> Items]
  /\ receipts \in [Files -> SUBSET Copies]
  /\ desired \in [Files -> Copies \cup {Absent}]
  /\ remaining \subseteq Files
  /\ external \subseteq Files
  /\ phase \in {"idle", "planned", "applying", "complete"}

UnownedPreserved == \A f \in external : disk[f] = Cargo

OwnedBeforePublished ==
  \A f \in Files : disk[f] \in Copies => disk[f] \in receipts[f]

CompleteIsCurrent ==
  phase = "complete" => \A f \in Files \ external :
  /\ disk[f] = desired[f]
  /\ receipts[f] = IF desired[f] = Absent THEN {} ELSE {desired[f]}

=============================================================================
