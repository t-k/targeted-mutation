--------------------------- MODULE NextestStop ----------------------------
VARIABLE phase, code, exited, checked, timedOut, outcome
vars == <<phase, code, exited, checked, timedOut, outcome>>
Codes == {"success", "test-failure", "runner-error"}
Init ==
    /\ phase = "running"
    /\ code = "pending"
    /\ exited = FALSE
    /\ checked = FALSE
    /\ timedOut = FALSE
    /\ outcome = "pending"

Exit == /\ phase \in {"running", "stopping", "forcing"}
        /\ code' \in Codes
        /\ phase' = "reaped"
        /\ exited' = TRUE
        /\ UNCHANGED <<checked, timedOut, outcome>>

Timeout == /\ phase = "running"
           /\ phase' = "stopping"
           /\ timedOut' = TRUE
           /\ UNCHANGED <<code, exited, checked, outcome>>

Escalate == /\ phase = "stopping"
            /\ phase' = "forcing"
            /\ UNCHANGED <<code, exited, checked, timedOut, outcome>>

CheckOwnedGroups == /\ phase = "reaped"
                    /\ phase' = "clean"
                    /\ checked' = TRUE
                    /\ UNCHANGED <<code, exited, timedOut, outcome>>

Publish ==
    /\ phase = "clean"
    /\ phase' = "done"
    /\ outcome' = IF timedOut THEN "timeout" ELSE code
    /\ UNCHANGED <<code, exited, checked, timedOut>>

Done == phase = "done" /\ UNCHANGED vars
Next == Exit \/ Timeout \/ Escalate \/ CheckOwnedGroups \/ Publish \/ Done
Spec == Init /\ [][Next]_vars

ReapBeforeResult == outcome # "pending" => exited /\ checked
OnlyTestFailureKills == outcome = "test-failure" => code = "test-failure" /\ ~timedOut
=============================================================================
