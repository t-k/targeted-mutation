---------------------------- MODULE SharedWitness ----------------------------
CONSTANT Inputs
VARIABLE step
vars == <<step>>
Init == step = [
    phase |-> "suites", source |-> "clean", survivors |-> {},
    raw |-> {}, confirmed |-> {}, reported |-> {}
]

Suites ==
    /\ step.phase = "suites"
    /\ \E passed \in SUBSET Inputs:
        step' = [step EXCEPT !.phase = "raw", !.source = "original", !.survivors = passed]

Raw ==
    /\ step.phase = "raw"
    /\ \E read \in SUBSET step.survivors:
        step' = [step EXCEPT !.phase = "typed", !.raw = read]

Typed ==
    /\ step.phase = "typed"
    /\ \E checked \in SUBSET step.raw:
        step' = [step EXCEPT !.phase = "restore", !.confirmed = checked]

Restore ==
    /\ step.phase = "restore"
    /\ \E restored \in BOOLEAN:
        step' = [step EXCEPT
            !.phase = IF restored THEN "compare" ELSE "blocked",
            !.source = IF restored THEN "clean" ELSE @]

Compare ==
    /\ step.phase = "compare"
    /\ \E shared \in SUBSET step.confirmed, fallback \in SUBSET step.survivors:
        /\ shared \cap fallback = {}
        /\ step' = [step EXCEPT !.phase = "done", !.reported = shared \cup fallback]

Finish == step.phase \in {"done", "blocked"} /\ UNCHANGED vars

Next == Suites \/ Raw \/ Typed \/ Restore \/ Compare \/ Finish
Spec == Init /\ [][Next]_vars

TypedValuesWereObserved == step.confirmed \subseteq step.raw
OnlySurvivingInputs == step.reported \subseteq step.survivors
ReportAfterRestore == step.phase \in {"compare", "done"} => step.source = "clean"
FailedRestorePublishesNothing == step.phase = "blocked" => step.reported = {}
=============================================================================
