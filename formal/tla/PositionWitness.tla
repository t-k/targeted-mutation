--------------------------- MODULE PositionWitness ---------------------------
EXTENDS Naturals

\* The source observation is attempted after a mutant survives the selected tests.
\* The restore step precedes publishing the witness or trying another candidate.
CONSTANT MaxCandidates
VARIABLES phase, source, remaining, suitePassed, original, mutant, reported

vars == <<phase, source, remaining, suitePassed, original, mutant, reported>>
Values == {"none", "first", "last"}
Missing == "missing"

Init ==
    /\ phase = "ready"
    /\ source = "restored"
    /\ remaining = MaxCandidates
    /\ suitePassed = FALSE
    /\ original = Missing
    /\ mutant = Missing
    /\ reported = FALSE

Start ==
    /\ phase = "ready"
    /\ remaining > 0
    /\ phase' = "suite"
    /\ source' = "mutant"
    /\ remaining' = remaining - 1
    /\ suitePassed' = FALSE
    /\ original' = Missing
    /\ mutant' = Missing
    /\ reported' = FALSE

Suite ==
    /\ phase = "suite"
    /\ \E passed, selectedLibrary \in BOOLEAN:
           /\ suitePassed' = passed
           /\ phase' = IF passed /\ selectedLibrary THEN "original" ELSE "restore"
           /\ source' = IF phase' = "original" THEN "originalTest" ELSE source
    /\ UNCHANGED <<remaining, original, mutant, reported>>

Original ==
    /\ phase = "original"
    /\ \E value \in Values \cup {Missing}:
           /\ original' = value
           /\ phase' = IF value = Missing THEN "restore" ELSE "mutant"
           /\ source' = IF phase' = "mutant" THEN "mutantTest" ELSE source
    /\ UNCHANGED <<remaining, suitePassed, mutant, reported>>

Mutant ==
    /\ phase = "mutant"
    /\ mutant' \in Values \cup {Missing}
    /\ phase' = "restore"
    /\ UNCHANGED <<source, remaining, suitePassed, original, reported>>

Restore ==
    /\ phase = "restore"
    /\ \E restored \in BOOLEAN:
           /\ phase' = IF restored THEN "ready" ELSE "blocked"
           /\ source' = IF restored THEN "restored" ELSE source
           /\ reported' = (restored /\ suitePassed /\ original \in Values /\ mutant \in Values /\ original # mutant)
    /\ UNCHANGED <<remaining, suitePassed, original, mutant>>

Recover ==
    /\ phase = "blocked"
    /\ phase' = "ready"
    /\ source' = "restored"
    /\ UNCHANGED <<remaining, suitePassed, original, mutant, reported>>

Done == phase = "ready" /\ remaining = 0 /\ UNCHANGED vars

Next == Start \/ Suite \/ Original \/ Mutant \/ Restore \/ Recover \/ Done
Spec == Init /\ [][Next]_vars

ReadyIsClean == phase = "ready" => source = "restored"
SuiteHasNoGeneratedTest == phase = "suite" => source = "mutant"
WitnessRequiresSurvivingMutant == phase \in {"original", "mutant"} => suitePassed
ReportHasEvidence ==
    reported => /\ phase = "ready"
                /\ suitePassed
                /\ original \in Values
                /\ mutant \in Values
                /\ original # mutant

=============================================================================
