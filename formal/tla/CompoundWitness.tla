--------------------------- MODULE CompoundWitness ---------------------------
EXTENDS Naturals

CONSTANT Attempts
VARIABLES phase, source, remaining, paired, leftKilled, rightKilled,
          suitePassed, original, mutant, confirmed, published
vars == <<phase, source, remaining, paired, leftKilled, rightKilled,
          suitePassed, original, mutant, confirmed, published>>
Values == {"first", "second"}

Init ==
    /\ phase = "ready"
    /\ source = "clean"
    /\ remaining = Attempts
    /\ paired = FALSE
    /\ leftKilled = FALSE
    /\ rightKilled = FALSE
    /\ suitePassed = FALSE
    /\ original = "missing"
    /\ mutant = "missing"
    /\ confirmed = FALSE
    /\ published = FALSE

Begin ==
    /\ phase = "ready"
    /\ remaining > 0
    /\ paired' \in BOOLEAN
    /\ phase' = IF paired' THEN "left" ELSE "suite"
    /\ source' = phase'
    /\ remaining' = remaining - 1
    /\ leftKilled' = FALSE
    /\ rightKilled' = FALSE
    /\ suitePassed' = FALSE
    /\ original' = "missing"
    /\ mutant' = "missing"
    /\ confirmed' = FALSE
    /\ published' = FALSE

Left ==
    /\ phase = "left"
    /\ leftKilled' \in BOOLEAN
    /\ phase' = IF leftKilled' THEN "right" ELSE "restore"
    /\ source' = IF leftKilled' THEN "right" ELSE source
    /\ UNCHANGED <<remaining, paired, rightKilled, suitePassed, original, mutant, confirmed, published>>

Right ==
    /\ phase = "right"
    /\ rightKilled' \in BOOLEAN
    /\ phase' = IF rightKilled' THEN "suite" ELSE "restore"
    /\ source' = IF rightKilled' THEN "suite" ELSE source
    /\ UNCHANGED <<remaining, paired, leftKilled, suitePassed, original, mutant, confirmed, published>>

Suite ==
    /\ phase = "suite"
    /\ suitePassed' \in BOOLEAN
    /\ phase' = IF suitePassed' THEN "original" ELSE "restore"
    /\ source' = IF suitePassed' THEN "original" ELSE source
    /\ UNCHANGED <<remaining, paired, leftKilled, rightKilled, original, mutant, confirmed, published>>

Original ==
    /\ phase = "original"
    /\ original' \in Values \cup {"missing"}
    /\ phase' = IF original' \in Values THEN "mutant" ELSE "restore"
    /\ source' = IF phase' = "mutant" THEN "mutant" ELSE source
    /\ UNCHANGED <<remaining, paired, leftKilled, rightKilled, suitePassed, mutant, confirmed, published>>

Mutant ==
    /\ phase = "mutant"
    /\ \E different \in BOOLEAN:
         /\ mutant' \in Values \cup {"missing"}
         /\ phase' = IF different /\ mutant' \in Values /\ mutant' # original THEN "confirm" ELSE "restore"
    /\ source' = IF phase' = "confirm" THEN "originalCheck" ELSE source
    /\ UNCHANGED <<remaining, paired, leftKilled, rightKilled, suitePassed, original, confirmed, published>>

Confirm ==
    /\ phase = "confirm"
    /\ confirmed' \in BOOLEAN
    /\ phase' = "restore"
    /\ UNCHANGED <<source, remaining, paired, leftKilled, rightKilled, suitePassed, original, mutant, published>>

Restore ==
    /\ phase = "restore"
    /\ \E restored \in BOOLEAN:
         /\ phase' = IF restored THEN "ready" ELSE "blocked"
         /\ source' = IF restored THEN "clean" ELSE source
         /\ published' = (restored /\ suitePassed /\ confirmed)
    /\ UNCHANGED <<remaining, paired, leftKilled, rightKilled, suitePassed, original, mutant, confirmed>>

Recover ==
    /\ phase = "blocked"
    /\ phase' = "ready"
    /\ source' = "clean"
    /\ UNCHANGED <<remaining, paired, leftKilled, rightKilled, suitePassed, original, mutant, confirmed, published>>

Done == phase = "ready" /\ remaining = 0 /\ UNCHANGED vars
Next == Begin \/ Left \/ Right \/ Suite \/ Original \/ Mutant \/ Confirm \/ Restore \/ Recover \/ Done
Spec == Init /\ [][Next]_vars

ReadyIsClean == phase = "ready" => source = "clean"
ComponentsBeforePair == paired /\ phase \in {"suite", "original", "mutant", "confirm"} => leftKilled /\ rightKilled
OriginalSuiteHasNoGeneratedTest == phase \in {"left", "right", "suite"} => source = phase
ReportHasEvidence ==
    published => /\ phase = "ready" /\ suitePassed /\ confirmed
                 /\ original \in Values /\ mutant \in Values /\ original # mutant
                 /\ (paired => leftKilled /\ rightKilled)
=============================================================================
