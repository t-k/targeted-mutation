--------------------------- MODULE WorkspaceRun ----------------------------
CONSTANT Members
VARIABLES remaining, reports, failures, active, output
vars == <<remaining, reports, failures, active, output>>

NoMember == "idle"
Init ==
    /\ remaining = Members
    /\ reports = {}
    /\ failures = {}
    /\ active = NoMember
    /\ output = FALSE

Start ==
    /\ active = NoMember
    /\ \E member \in remaining:
        /\ active' = member
        /\ remaining' = remaining \ {member}
    /\ UNCHANGED <<reports, failures, output>>

Finish ==
    /\ active \in Members
    /\ \/ /\ reports' = reports \cup {active}
          /\ UNCHANGED failures
       \/ /\ failures' = failures \cup {active}
          /\ UNCHANGED reports
    /\ active' = NoMember
    /\ UNCHANGED <<remaining, output>>

Publish ==
    /\ active = NoMember
    /\ remaining = {}
    /\ output' = TRUE
    /\ UNCHANGED <<remaining, reports, failures, active>>

Next == Start \/ Finish \/ Publish
Spec == Init /\ [][Next]_vars

ExactlyOneState ==
    /\ reports \cap failures = {}
    /\ (reports \cup failures) \cap remaining = {}
    /\ active \notin (reports \cup failures \cup remaining)
    /\ Members = reports \cup failures \cup remaining \cup ({active} \cap Members)

CompleteOutput == output => reports \cup failures = Members
=============================================================================
